//! One IR function to one wasm function: every `Value` lives in a typed local
//! (values whose live ranges never overlap share one: locals.rs, D3228),
//! an edge is a parallel copy into the target's parameter locals (all
//! arguments pushed, then set in reverse), and the CFG is stackified with
//! Ramsey's "Beyond Relooper" (ICFP 2022 §4–5; the GHC wasm backend's
//! `GHC.CmmToAsm.Wasm.FromCmm` is the implementation followed): a merge
//! node gets a `block` ending where it starts, a loop header a `loop`, a
//! block with one forward predecessor is emitted inline at its branch.

use crate::cfg::{Cfg, CfgError};
use crate::{frame_layout, host_sig, irreduce, val_type, Helper, Refusal, Types};
use gravity_ir::*;
use wasm_encoder::{BlockType, Function as WasmFunction, Ieee32, Ieee64, Instruction as I, MemArg, ValType};

enum Label {
    Block(Block),
    Loop(Block),
    If,
}

struct Fx<'a> {
    program: &'a Program,
    func: Func,
    f: &'a Function,
    cfg: Cfg,
    loc: Vec<u32>,
    out: WasmFunction,
    labels: Vec<Label>,
    types: &'a mut Types,
    helpers: &'a mut Vec<Helper>,
    nfuncs: u32,
    fp: u32,
    sp_save: u32,
    t32: [u32; 2],
    t64: [u32; 2],
    tv: [u32; 2],
    offsets: Vec<u32>,
    frame: u32,
    guard: Option<crate::StackGuard>,
    link: &'a crate::Link,
}

pub(crate) fn emit<'a>(program: &'a Program, func: Func, f: &'a Function, types: &'a mut Types, helpers: &'a mut Vec<Helper>, nfuncs: u32, guard: Option<crate::StackGuard>, link: &'a crate::Link) -> Result<WasmFunction, Refusal> {
    let fixed;
    let (f, cfg) = match Cfg::build(f) {
        Ok(cfg) => (f, cfg),
        Err(CfgError::Irreducible { from, to }) => {
            let refusal = Refusal::Irreducible { func, from, to };
            fixed = irreduce::make_reducible(f).map_err(|_| refusal.clone())?.ok_or_else(|| refusal.clone())?;
            let cfg = Cfg::build(&fixed.funcs[0]).map_err(|_| refusal)?;
            (&fixed.funcs[0], cfg)
        }
    };
    let np = f.block(f.entry).params.len() as u32;
    // D3228: a local per live range (locals.rs), so a wide function stays under the engines' locals limit
    let (loc, mut locals) = crate::locals::assign(f, &cfg);
    let mut scratch = |t: ValType| {
        locals.push(t);
        np + locals.len() as u32 - 1
    };
    let fp = scratch(ValType::I32);
    let sp_save = scratch(ValType::I32);
    let t32 = [scratch(ValType::I32), scratch(ValType::I32)];
    let t64 = [scratch(ValType::I64), scratch(ValType::I64)];
    let tv = [scratch(ValType::V128), scratch(ValType::V128)];
    let (offsets, frame) = frame_layout(f);
    let mut fx = Fx { program, func, f, cfg, loc, out: WasmFunction::new_with_locals_types(locals), labels: Vec::new(), types, helpers, nfuncs, fp, sp_save, t32, t64, tv, offsets, frame, guard, link };
    if fx.frame > 0 {
        fx.i(I::GlobalGet(0));
        fx.i(I::LocalTee(fx.sp_save));
        fx.i(I::I32Const(fx.frame as i32));
        fx.i(I::I32Sub);
        fx.i(I::LocalTee(fx.fp));
        // the shared shadow stack's floor (`StackGuard`): a frame below it calls the server's overflow host, which raises
        // (an unwind through this frame, the stack pointer not yet moved) instead of writing below the stack or wrapping
        if let Some(g) = fx.guard {
            fx.i(I::Drop);
            fx.i(I::LocalGet(fx.fp));
            fx.i(I::I32Const(g.floor as i32));
            fx.i(I::I32LtU);
            fx.i(I::LocalGet(fx.fp));
            fx.i(I::LocalGet(fx.sp_save));
            fx.i(I::I32GtU); // the subtraction wrapped
            fx.i(I::I32Or);
            fx.i(I::If(BlockType::Empty));
            match fx.link.overflow {
                Some(k) => fx.i(I::Call(k)),
                None => {
                    fx.i(I::I32Const(g.overflow as i32));
                    let type_index = fx.types.index(&[], &[]);
                    fx.i(I::CallIndirect { type_index, table_index: 0 });
                }
            }
            fx.i(I::End);
            fx.i(I::LocalGet(fx.fp));
        }
        fx.i(I::GlobalSet(0));
    }
    fx.do_tree(f.entry)?;
    fx.i(I::Unreachable);
    fx.i(I::End);
    Ok(fx.out)
}

pub(crate) fn entry_wrapper(entry: Func, base: u32) -> WasmFunction {
    let mut w = WasmFunction::new([]);
    w.instruction(&I::LocalGet(0));
    w.instruction(&I::I64ExtendI32U);
    w.instruction(&I::Call(base + entry.0));
    w.instruction(&I::End);
    w
}

impl<'a> Fx<'a> {
    fn i(&mut self, ins: I<'_>) {
        self.out.instruction(&ins);
    }

    fn get(&mut self, v: Value) {
        self.i(I::LocalGet(self.loc[v.index()]));
    }

    fn set(&mut self, v: Value) {
        self.i(I::LocalSet(self.loc[v.index()]));
    }

    fn ty(&self, v: Value) -> Type {
        self.f.value_type(v)
    }

    fn label_depth(&self, want: impl Fn(&Label) -> bool) -> u32 {
        let pos = self.labels.iter().rposition(want).expect("stackifier: label in scope");
        (self.labels.len() - 1 - pos) as u32
    }

    // ---------------------------------------------------------------- control flow

    fn do_tree(&mut self, x: Block) -> Result<(), Refusal> {
        let mut children: Vec<Block> = self.cfg.children[x.index()].iter().copied().filter(|&y| self.cfg.merge[y.index()]).collect();
        children.sort_by_key(|&y| core::cmp::Reverse(self.cfg.rpo_num[y.index()]));
        let is_loop = self.cfg.loop_header[x.index()];
        if is_loop {
            self.i(I::Loop(BlockType::Empty));
            self.labels.push(Label::Loop(x));
        }
        self.node_within(x, &children)?;
        if is_loop {
            self.labels.pop();
            self.i(I::End);
            self.i(I::Unreachable);
        }
        Ok(())
    }

    fn node_within(&mut self, x: Block, children: &[Block]) -> Result<(), Refusal> {
        stack_depth_core::check_stack_depth_or_panic();
        if let Some((&y, rest)) = children.split_first() {
            self.i(I::Block(BlockType::Empty));
            self.labels.push(Label::Block(y));
            self.node_within(x, rest)?;
            self.labels.pop();
            self.i(I::End);
            return self.do_tree(y);
        }
        let b = self.f.block(x);
        for inst in &b.insts {
            self.inst(x, inst)?;
        }
        match &b.term {
            Terminator::Br(e) => self.do_branch(x, e)?,
            Terminator::CondBr { cond, then_edge, else_edge } => {
                self.get(*cond);
                self.i(I::If(BlockType::Empty));
                self.labels.push(Label::If);
                self.do_branch(x, then_edge)?;
                self.i(I::Else);
                self.do_branch(x, else_edge)?;
                self.labels.pop();
                self.i(I::End);
                self.i(I::Unreachable);
            }
            Terminator::Switch { value, cases, default } => {
                let ty = self.ty(*value);
                for c in cases {
                    self.get(*value);
                    self.konst(ty, c.value);
                    self.i(if val_type(ty) == ValType::I64 { I::I64Eq } else { I::I32Eq });
                    self.i(I::If(BlockType::Empty));
                    self.labels.push(Label::If);
                    self.do_branch(x, &c.edge)?;
                    self.i(I::Else);
                }
                self.do_branch(x, default)?;
                for _ in cases {
                    self.labels.pop();
                    self.i(I::End);
                }
                self.i(I::Unreachable);
            }
            Terminator::Return(vals) => {
                if self.frame > 0 {
                    self.i(I::LocalGet(self.sp_save));
                    self.i(I::GlobalSet(0));
                }
                for &v in vals {
                    self.get(v);
                }
                self.i(I::Return);
            }
            Terminator::Unreachable => self.i(I::Unreachable),
        }
        Ok(())
    }

    fn do_branch(&mut self, x: Block, e: &Edge) -> Result<(), Refusal> {
        let t = e.target;
        for &a in &e.args {
            self.get(a);
        }
        let params = self.f.block(t).params.to_vec();
        for &p in params.iter().rev() {
            self.set(p);
        }
        if self.cfg.is_back_edge(x, t) {
            let d = self.label_depth(|l| matches!(l, Label::Loop(b) if *b == t));
            self.i(I::Br(d));
        } else if self.cfg.merge[t.index()] {
            let d = self.label_depth(|l| matches!(l, Label::Block(b) if *b == t));
            self.i(I::Br(d));
        } else {
            self.do_tree(t)?;
        }
        Ok(())
    }

    // ---------------------------------------------------------------- scalars

    fn konst(&mut self, ty: Type, bits: u64) {
        match ty {
            Type::Bool => self.i(I::I32Const((bits & 1) as i32)),
            Type::I8 => self.i(I::I32Const(bits as u8 as i8 as i32)),
            Type::I16 => self.i(I::I32Const(bits as u16 as i16 as i32)),
            Type::I32 => self.i(I::I32Const(bits as u32 as i32)),
            Type::I64 | Type::Ptr => self.i(I::I64Const(bits as i64)),
            Type::F32 => self.i(I::F32Const(Ieee32::new(bits as u32))),
            Type::F64 => self.i(I::F64Const(Ieee64::new(bits))),
            _ => unreachable!("verified: scalar constant"),
        }
    }

    /// Re-establish the canonical form of a narrow result on the stack.
    fn canon(&mut self, ty: Type) {
        match ty {
            Type::I8 => self.i(I::I32Extend8S),
            Type::I16 => self.i(I::I32Extend16S),
            Type::Bool => {
                self.i(I::I32Const(1));
                self.i(I::I32And);
            }
            _ => {}
        }
    }

    /// The zero-extended view of a narrow value on the stack.
    fn zext(&mut self, ty: Type) {
        match ty {
            Type::I8 => {
                self.i(I::I32Const(0xff));
                self.i(I::I32And);
            }
            Type::I16 => {
                self.i(I::I32Const(0xffff));
                self.i(I::I32And);
            }
            _ => {}
        }
    }

    fn is64(ty: Type) -> bool {
        matches!(ty, Type::I64 | Type::Ptr)
    }

    fn wide(&mut self, ty: Type, i32_form: I<'static>, i64_form: I<'static>) {
        self.i(if Self::is64(ty) { i64_form } else { i32_form });
    }

    fn memarg(&mut self, ptr: Value, offset: i32, bytes: u32, unaligned: bool) -> MemArg {
        self.get(ptr);
        self.i(I::I32WrapI64);
        let off = if offset < 0 {
            self.i(I::I32Const(offset));
            self.i(I::I32Add);
            0
        } else {
            offset as u64
        };
        MemArg { offset: off, align: if unaligned { 0 } else { bytes.trailing_zeros() }, memory_index: 0 }
    }

    fn helper_index(&mut self, h: Helper) -> u32 {
        let pos = match self.helpers.iter().position(|&x| x == h) {
            Some(p) => p,
            None => {
                self.helpers.push(h);
                self.helpers.len() - 1
            }
        };
        self.link.base + self.nfuncs + pos as u32
    }

    fn int_bin(&mut self, op: IntBinOp, a: Value, b: Value) {
        let ty = self.ty(a);
        let narrow = matches!(ty, Type::I8 | Type::I16);
        let w = ty.bytes() * 8;
        match op {
            IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul | IntBinOp::And | IntBinOp::Or | IntBinOp::Xor | IntBinOp::Sdiv | IntBinOp::Srem => {
                self.get(a);
                self.get(b);
                let (i32f, i64f) = match op {
                    IntBinOp::Add => (I::I32Add, I::I64Add),
                    IntBinOp::Sub => (I::I32Sub, I::I64Sub),
                    IntBinOp::Mul => (I::I32Mul, I::I64Mul),
                    IntBinOp::And => (I::I32And, I::I64And),
                    IntBinOp::Or => (I::I32Or, I::I64Or),
                    IntBinOp::Xor => (I::I32Xor, I::I64Xor),
                    IntBinOp::Sdiv => (I::I32DivS, I::I64DivS),
                    _ => (I::I32RemS, I::I64RemS),
                };
                self.wide(ty, i32f, i64f);
                if narrow && !matches!(op, IntBinOp::And | IntBinOp::Or | IntBinOp::Xor | IntBinOp::Srem) {
                    self.canon(ty);
                }
            }
            IntBinOp::Udiv | IntBinOp::Urem => {
                self.get(a);
                self.zext(ty);
                self.get(b);
                self.zext(ty);
                self.wide(ty, if op == IntBinOp::Udiv { I::I32DivU } else { I::I32RemU }, if op == IntBinOp::Udiv { I::I64DivU } else { I::I64RemU });
                self.canon(ty);
            }
            IntBinOp::Shl | IntBinOp::Ushr | IntBinOp::Sshr => {
                self.get(a);
                if op == IntBinOp::Ushr {
                    self.zext(ty);
                }
                self.get(b);
                if narrow {
                    self.i(I::I32Const(w as i32 - 1));
                    self.i(I::I32And);
                }
                match op {
                    IntBinOp::Shl => self.wide(ty, I::I32Shl, I::I64Shl),
                    IntBinOp::Ushr => self.wide(ty, I::I32ShrU, I::I64ShrU),
                    _ => self.wide(ty, I::I32ShrS, I::I64ShrS),
                }
                if op != IntBinOp::Sshr {
                    self.canon(ty);
                }
            }
            IntBinOp::Rotl => {
                // I64 only (verify); the count is taken modulo 64 by the instruction
                self.get(a);
                self.get(b);
                self.i(I::I64Rotl);
            }
            IntBinOp::Smulh | IntBinOp::Umulh => {
                self.get(a);
                self.get(b);
                let h = self.helper_index(if op == IntBinOp::Smulh { Helper::Smulh64 } else { Helper::Umulh64 });
                self.i(I::Call(h));
            }
            IntBinOp::Smin | IntBinOp::Smax | IntBinOp::Umin | IntBinOp::Umax => {
                self.get(a);
                self.get(b);
                self.get(a);
                self.get(b);
                match op {
                    IntBinOp::Smin => self.wide(ty, I::I32LtS, I::I64LtS),
                    IntBinOp::Smax => self.wide(ty, I::I32GtS, I::I64GtS),
                    IntBinOp::Umin => self.wide(ty, I::I32LtU, I::I64LtU),
                    _ => self.wide(ty, I::I32GtU, I::I64GtU),
                }
                self.i(I::Select);
            }
        }
    }

    fn int_ovf(&mut self, op: IntOvfOp, a: Value, b: Value) {
        let ty = self.ty(a);
        match (op, ty) {
            (IntOvfOp::SmulOvf, Type::I64) => {
                self.get(a);
                self.get(b);
                let h = self.helper_index(Helper::SmulOvf64);
                self.i(I::Call(h));
            }
            (IntOvfOp::SmulOvf, Type::I32) => {
                self.get(a);
                self.i(I::I64ExtendI32S);
                self.get(b);
                self.i(I::I64ExtendI32S);
                self.i(I::I64Mul);
                self.i(I::LocalTee(self.t64[0]));
                self.i(I::I32WrapI64);
                self.i(I::I64ExtendI32S);
                self.i(I::LocalGet(self.t64[0]));
                self.i(I::I64Ne);
            }
            (IntOvfOp::SmulOvf, _) | (_, Type::I8) | (_, Type::I16) => {
                self.get(a);
                self.get(b);
                self.i(match op {
                    IntOvfOp::SaddOvf => I::I32Add,
                    IntOvfOp::SsubOvf => I::I32Sub,
                    IntOvfOp::SmulOvf => I::I32Mul,
                });
                self.i(I::LocalTee(self.t32[0]));
                self.canon(ty);
                self.i(I::LocalGet(self.t32[0]));
                self.i(I::I32Ne);
            }
            (IntOvfOp::SaddOvf, _) => {
                // ((a ^ r) & (b ^ r)) < 0
                self.get(a);
                self.get(b);
                self.wide(ty, I::I32Add, I::I64Add);
                self.sign_ovf(ty, a, b, true);
            }
            (IntOvfOp::SsubOvf, _) => {
                // ((a ^ b) & (a ^ r)) < 0
                self.get(a);
                self.get(b);
                self.wide(ty, I::I32Sub, I::I64Sub);
                self.sign_ovf(ty, a, b, false);
            }
        }
    }

    fn sign_ovf(&mut self, ty: Type, a: Value, b: Value, add: bool) {
        let t = if Self::is64(ty) { self.t64[0] } else { self.t32[0] };
        self.i(I::LocalSet(t));
        self.get(a);
        if add {
            self.i(I::LocalGet(t));
        } else {
            self.get(b);
        }
        self.wide(ty, I::I32Xor, I::I64Xor);
        if add {
            self.get(b);
        } else {
            self.get(a);
        }
        self.i(I::LocalGet(t));
        self.wide(ty, I::I32Xor, I::I64Xor);
        self.wide(ty, I::I32And, I::I64And);
        if Self::is64(ty) {
            self.i(I::I64Const(0));
            self.i(I::I64LtS);
        } else {
            self.i(I::I32Const(0));
            self.i(I::I32LtS);
        }
    }

    fn int_un(&mut self, op: IntUnOp, a: Value) {
        let ty = self.ty(a);
        let w = ty.bytes() * 8;
        match op {
            IntUnOp::Neg => {
                if Self::is64(ty) {
                    self.i(I::I64Const(0));
                    self.get(a);
                    self.i(I::I64Sub);
                } else {
                    self.i(I::I32Const(0));
                    self.get(a);
                    self.i(I::I32Sub);
                    self.canon(ty);
                }
            }
            IntUnOp::Not => {
                self.get(a);
                if Self::is64(ty) {
                    self.i(I::I64Const(-1));
                    self.i(I::I64Xor);
                } else {
                    self.i(I::I32Const(-1));
                    self.i(I::I32Xor);
                }
            }
            IntUnOp::Clz => {
                self.get(a);
                self.zext(ty);
                self.wide(ty, I::I32Clz, I::I64Clz);
                if w < 32 {
                    self.i(I::I32Const(32 - w as i32));
                    self.i(I::I32Sub);
                }
            }
            IntUnOp::Ctz => {
                self.get(a);
                if w < 32 {
                    self.i(I::I32Const(1 << w));
                    self.i(I::I32Or);
                }
                self.wide(ty, I::I32Ctz, I::I64Ctz);
            }
            IntUnOp::Popcnt => {
                self.get(a);
                self.zext(ty);
                self.wide(ty, I::I32Popcnt, I::I64Popcnt);
            }
            IntUnOp::Bswap => {
                self.get(a);
                self.bswap(ty);
            }
            IntUnOp::Bitrev => {
                self.get(a);
                self.zext(ty);
                if Self::is64(ty) {
                    for (m, s) in [(0x5555_5555_5555_5555u64, 1), (0x3333_3333_3333_3333, 2), (0x0f0f_0f0f_0f0f_0f0f, 4)] {
                        self.i(I::LocalTee(self.t64[0]));
                        self.i(I::I64Const(m as i64));
                        self.i(I::I64And);
                        self.i(I::I64Const(s));
                        self.i(I::I64Shl);
                        self.i(I::LocalGet(self.t64[0]));
                        self.i(I::I64Const(s));
                        self.i(I::I64ShrU);
                        self.i(I::I64Const(m as i64));
                        self.i(I::I64And);
                        self.i(I::I64Or);
                    }
                } else {
                    for (m, s) in [(0x5555_5555u32, 1), (0x3333_3333, 2), (0x0f0f_0f0f, 4)] {
                        self.i(I::LocalTee(self.t32[0]));
                        self.i(I::I32Const(m as i32));
                        self.i(I::I32And);
                        self.i(I::I32Const(s));
                        self.i(I::I32Shl);
                        self.i(I::LocalGet(self.t32[0]));
                        self.i(I::I32Const(s));
                        self.i(I::I32ShrU);
                        self.i(I::I32Const(m as i32));
                        self.i(I::I32And);
                        self.i(I::I32Or);
                    }
                }
                self.bswap(ty);
            }
        }
    }

    /// Byte-reverse the value on the stack within its width (canonical result).
    fn bswap(&mut self, ty: Type) {
        match ty {
            Type::I8 => self.canon(ty),
            Type::I16 => {
                self.i(I::LocalTee(self.t32[0]));
                self.i(I::I32Const(0xff));
                self.i(I::I32And);
                self.i(I::I32Const(8));
                self.i(I::I32Shl);
                self.i(I::LocalGet(self.t32[0]));
                self.i(I::I32Const(8));
                self.i(I::I32ShrU);
                self.i(I::I32Const(0xff));
                self.i(I::I32And);
                self.i(I::I32Or);
                self.i(I::I32Extend16S);
            }
            Type::I32 => {
                self.i(I::LocalTee(self.t32[0]));
                self.i(I::I32Const(0xff00_ff00u32 as i32));
                self.i(I::I32And);
                self.i(I::I32Const(8));
                self.i(I::I32ShrU);
                self.i(I::LocalGet(self.t32[0]));
                self.i(I::I32Const(0x00ff_00ff));
                self.i(I::I32And);
                self.i(I::I32Const(8));
                self.i(I::I32Shl);
                self.i(I::I32Or);
                self.i(I::I32Const(16));
                self.i(I::I32Rotl);
            }
            _ => {
                for (m1, m2, s) in [(0xff00_ff00_ff00_ff00u64, 0x00ff_00ff_00ff_00ffu64, 8), (0xffff_0000_ffff_0000, 0x0000_ffff_0000_ffff, 16)] {
                    self.i(I::LocalTee(self.t64[0]));
                    self.i(I::I64Const(m1 as i64));
                    self.i(I::I64And);
                    self.i(I::I64Const(s));
                    self.i(I::I64ShrU);
                    self.i(I::LocalGet(self.t64[0]));
                    self.i(I::I64Const(m2 as i64));
                    self.i(I::I64And);
                    self.i(I::I64Const(s));
                    self.i(I::I64Shl);
                    self.i(I::I64Or);
                }
                self.i(I::I64Const(32));
                self.i(I::I64Rotl);
            }
        }
    }

    fn icmp(&mut self, cond: IntCond, a: Value, b: Value) {
        let ty = self.ty(a);
        self.get(a);
        self.get(b);
        let (i32f, i64f) = match cond {
            IntCond::Eq => (I::I32Eq, I::I64Eq),
            IntCond::Ne => (I::I32Ne, I::I64Ne),
            IntCond::Slt => (I::I32LtS, I::I64LtS),
            IntCond::Sle => (I::I32LeS, I::I64LeS),
            IntCond::Sgt => (I::I32GtS, I::I64GtS),
            IntCond::Sge => (I::I32GeS, I::I64GeS),
            IntCond::Ult => (I::I32LtU, I::I64LtU),
            IntCond::Ule => (I::I32LeU, I::I64LeU),
            IntCond::Ugt => (I::I32GtU, I::I64GtU),
            IntCond::Uge => (I::I32GeU, I::I64GeU),
        };
        self.wide(ty, i32f, i64f);
    }

    fn float_bin(&mut self, op: FloatBinOp, a: Value, b: Value) {
        let f32_ = self.ty(a) == Type::F32;
        match op {
            FloatBinOp::Min | FloatBinOp::Max => {
                // Rust's min/max: the non-NaN operand when one is NaN.
                self.get(b);
                self.get(a);
                self.get(a);
                self.get(b);
                self.i(match (op, f32_) {
                    (FloatBinOp::Min, true) => I::F32Min,
                    (FloatBinOp::Min, false) => I::F64Min,
                    (_, true) => I::F32Max,
                    (_, false) => I::F64Max,
                });
                self.get(b);
                self.get(b);
                self.i(if f32_ { I::F32Ne } else { I::F64Ne });
                self.i(I::Select);
                self.get(a);
                self.get(a);
                self.i(if f32_ { I::F32Ne } else { I::F64Ne });
                self.i(I::Select);
            }
            _ => {
                self.get(a);
                self.get(b);
                self.i(match (op, f32_) {
                    (FloatBinOp::Add, true) => I::F32Add,
                    (FloatBinOp::Add, false) => I::F64Add,
                    (FloatBinOp::Sub, true) => I::F32Sub,
                    (FloatBinOp::Sub, false) => I::F64Sub,
                    (FloatBinOp::Mul, true) => I::F32Mul,
                    (FloatBinOp::Mul, false) => I::F64Mul,
                    (FloatBinOp::Div, true) => I::F32Div,
                    (FloatBinOp::Div, false) => I::F64Div,
                    (_, true) => I::F32Copysign,
                    (_, false) => I::F64Copysign,
                });
            }
        }
    }

    fn float_un(&mut self, op: FloatUnOp, a: Value) {
        let f32_ = self.ty(a) == Type::F32;
        self.get(a);
        self.i(match (op, f32_) {
            (FloatUnOp::Neg, true) => I::F32Neg,
            (FloatUnOp::Neg, false) => I::F64Neg,
            (FloatUnOp::Abs, true) => I::F32Abs,
            (FloatUnOp::Abs, false) => I::F64Abs,
            (FloatUnOp::Sqrt, true) => I::F32Sqrt,
            (FloatUnOp::Sqrt, false) => I::F64Sqrt,
            (FloatUnOp::Ceil, true) => I::F32Ceil,
            (FloatUnOp::Ceil, false) => I::F64Ceil,
            (FloatUnOp::Floor, true) => I::F32Floor,
            (FloatUnOp::Floor, false) => I::F64Floor,
            (FloatUnOp::Trunc, true) => I::F32Trunc,
            (FloatUnOp::Trunc, false) => I::F64Trunc,
            (FloatUnOp::Nearest, true) => I::F32Nearest,
            (FloatUnOp::Nearest, false) => I::F64Nearest,
        });
    }

    fn fcmp(&mut self, cond: FloatCond, a: Value, b: Value) {
        let f32_ = self.ty(a) == Type::F32;
        let eq = if f32_ { I::F32Eq } else { I::F64Eq };
        let ne = if f32_ { I::F32Ne } else { I::F64Ne };
        match cond {
            FloatCond::Ordered | FloatCond::Unordered => {
                let ordered = cond == FloatCond::Ordered;
                self.get(a);
                self.get(a);
                self.i(if ordered { eq.clone() } else { ne.clone() });
                self.get(b);
                self.get(b);
                self.i(if ordered { eq } else { ne });
                self.i(if ordered { I::I32And } else { I::I32Or });
            }
            _ => {
                self.get(a);
                self.get(b);
                self.i(match (cond, f32_) {
                    (FloatCond::Eq, true) => I::F32Eq,
                    (FloatCond::Eq, false) => I::F64Eq,
                    (FloatCond::Ne, true) => I::F32Ne,
                    (FloatCond::Ne, false) => I::F64Ne,
                    (FloatCond::Lt, true) => I::F32Lt,
                    (FloatCond::Lt, false) => I::F64Lt,
                    (FloatCond::Le, true) => I::F32Le,
                    (FloatCond::Le, false) => I::F64Le,
                    (FloatCond::Gt, true) => I::F32Gt,
                    (FloatCond::Gt, false) => I::F64Gt,
                    (_, true) => I::F32Ge,
                    (_, false) => I::F64Ge,
                });
            }
        }
    }

    fn conv(&mut self, op: ConvOp, to: Type, a: Value) {
        let from = self.ty(a);
        if from.is_vector() {
            self.get(a);
            return;
        }
        self.get(a);
        match op {
            ConvOp::Sext => {
                if Self::is64(to) {
                    self.i(I::I64ExtendI32S);
                }
            }
            ConvOp::Zext => {
                self.zext(from);
                if Self::is64(to) {
                    self.i(I::I64ExtendI32U);
                }
            }
            ConvOp::Trunc => {
                if Self::is64(from) {
                    self.i(I::I32WrapI64);
                }
                self.canon(to);
            }
            ConvOp::SintToFloat => self.i(match (Self::is64(from), to) {
                (false, Type::F32) => I::F32ConvertI32S,
                (false, _) => I::F64ConvertI32S,
                (true, Type::F32) => I::F32ConvertI64S,
                (true, _) => I::F64ConvertI64S,
            }),
            ConvOp::UintToFloat => {
                self.zext(from);
                self.i(match (Self::is64(from), to) {
                    (false, Type::F32) => I::F32ConvertI32U,
                    (false, _) => I::F64ConvertI32U,
                    (true, Type::F32) => I::F32ConvertI64U,
                    (true, _) => I::F64ConvertI64U,
                });
            }
            ConvOp::FloatToSint | ConvOp::FloatToUint => {
                // Saturate at 64 bits, then truncate to the width (the interpreter's `as i64`/`as u64`).
                let signed = op == ConvOp::FloatToSint;
                self.i(match (from, signed) {
                    (Type::F32, true) => I::I64TruncSatF32S,
                    (Type::F32, false) => I::I64TruncSatF32U,
                    (_, true) => I::I64TruncSatF64S,
                    (_, false) => I::I64TruncSatF64U,
                });
                if !Self::is64(to) {
                    self.i(I::I32WrapI64);
                    self.canon(to);
                }
            }
            ConvOp::Fpromote => self.i(I::F64PromoteF32),
            ConvOp::Fdemote => self.i(I::F32DemoteF64),
            ConvOp::Bitcast => match (from, to) {
                (Type::I32, Type::F32) => self.i(I::F32ReinterpretI32),
                (Type::F32, Type::I32) => self.i(I::I32ReinterpretF32),
                (Type::I64, Type::F64) => self.i(I::F64ReinterpretI64),
                (Type::F64, Type::I64) => self.i(I::I64ReinterpretF64),
                _ => {}
            },
            ConvOp::PtrToInt | ConvOp::IntToPtr => {}
        }
    }

    fn load(&mut self, ty: Type, ptr: Value, offset: i32, unaligned: bool, zext64: bool) {
        let m = self.memarg(ptr, offset, ty.bytes(), unaligned);
        self.i(match (ty, zext64) {
            (Type::I8, true) => I::I64Load8U(m),
            (Type::I16, true) => I::I64Load16U(m),
            (Type::I32, true) => I::I64Load32U(m),
            (Type::Bool, _) => I::I32Load8U(m),
            (Type::I8, _) => I::I32Load8S(m),
            (Type::I16, _) => I::I32Load16S(m),
            (Type::I32, _) => I::I32Load(m),
            (Type::I64 | Type::Ptr, _) => I::I64Load(m),
            (Type::F32, _) => I::F32Load(m),
            (Type::F64, _) => I::F64Load(m),
            _ => I::V128Load(m),
        });
    }

    fn store(&mut self, ptr: Value, offset: i32, value: Value, unaligned: bool) {
        let ty = self.ty(value);
        let m = self.memarg(ptr, offset, ty.bytes(), unaligned);
        self.get(value);
        self.i(match ty {
            Type::Bool | Type::I8 => I::I32Store8(m),
            Type::I16 => I::I32Store16(m),
            Type::I32 => I::I32Store(m),
            Type::I64 | Type::Ptr => I::I64Store(m),
            Type::F32 => I::F32Store(m),
            Type::F64 => I::F64Store(m),
            _ => I::V128Store(m),
        });
    }

    // ---------------------------------------------------------------- vectors

    fn lanes_op(ty: Type, forms: [I<'static>; 6]) -> I<'static> {
        let [a, b, c, d, e, f] = forms;
        match ty {
            Type::V16I8 => a,
            Type::V8I16 => b,
            Type::V4I32 => c,
            Type::V2I64 => d,
            Type::V4F32 => e,
            _ => f,
        }
    }

    fn vsplat_const(&mut self, ty: Type, lane_bits: u64) {
        let w = 128 / ty.lanes();
        let mut v: u128 = 0;
        for k in 0..ty.lanes() {
            v |= ((lane_bits as u128) & ((1u128 << w) - 1)) << (k * w);
        }
        self.i(I::V128Const(v as i128));
    }

    fn vec_bin(&mut self, block: Block, op: VecBinOp, a: Value, b: Value) -> Result<(), Refusal> {
        let ty = self.ty(a);
        let func = self.func;
        let unsupported = move |what| Refusal::Unsupported { func, block, what };
        match op {
            VecBinOp::And | VecBinOp::Or | VecBinOp::Xor => {
                self.get(a);
                self.get(b);
                self.i(match op {
                    VecBinOp::And => I::V128And,
                    VecBinOp::Or => I::V128Or,
                    _ => I::V128Xor,
                });
            }
            VecBinOp::Add => {
                self.get(a);
                self.get(b);
                self.i(Self::lanes_op(ty, [I::I8x16Add, I::I16x8Add, I::I32x4Add, I::I64x2Add, I::F32x4Add, I::F64x2Add]));
            }
            VecBinOp::Sub => {
                self.get(a);
                self.get(b);
                self.i(Self::lanes_op(ty, [I::I8x16Sub, I::I16x8Sub, I::I32x4Sub, I::I64x2Sub, I::F32x4Sub, I::F64x2Sub]));
            }
            VecBinOp::Mul if ty == Type::V16I8 => {
                self.get(a);
                self.get(b);
                self.i(I::I16x8ExtMulLowI8x16U);
                self.vsplat_const(Type::V8I16, 0xff);
                self.i(I::V128And);
                self.get(a);
                self.get(b);
                self.i(I::I16x8ExtMulHighI8x16U);
                self.vsplat_const(Type::V8I16, 0xff);
                self.i(I::V128And);
                self.i(I::I8x16NarrowI16x8U);
            }
            VecBinOp::Mul => {
                self.get(a);
                self.get(b);
                self.i(Self::lanes_op(ty, [I::Nop, I::I16x8Mul, I::I32x4Mul, I::I64x2Mul, I::F32x4Mul, I::F64x2Mul]));
            }
            VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax => {
                if !ty.is_int_vector() {
                    return Err(unsupported("vector min/max on float lanes"));
                }
                if ty == Type::V2I64 {
                    let signed = matches!(op, VecBinOp::Smin | VecBinOp::Smax);
                    self.get(a);
                    self.get(b);
                    self.get(a);
                    if !signed {
                        self.vsplat_const(ty, 1 << 63);
                        self.i(I::V128Xor);
                    }
                    self.get(b);
                    if !signed {
                        self.vsplat_const(ty, 1 << 63);
                        self.i(I::V128Xor);
                    }
                    self.i(if matches!(op, VecBinOp::Smin | VecBinOp::Umin) { I::I64x2LtS } else { I::I64x2GtS });
                    self.i(I::V128Bitselect);
                } else {
                    self.get(a);
                    self.get(b);
                    self.i(match op {
                        VecBinOp::Smin => Self::lanes_op(ty, [I::I8x16MinS, I::I16x8MinS, I::I32x4MinS, I::Nop, I::Nop, I::Nop]),
                        VecBinOp::Smax => Self::lanes_op(ty, [I::I8x16MaxS, I::I16x8MaxS, I::I32x4MaxS, I::Nop, I::Nop, I::Nop]),
                        VecBinOp::Umin => Self::lanes_op(ty, [I::I8x16MinU, I::I16x8MinU, I::I32x4MinU, I::Nop, I::Nop, I::Nop]),
                        _ => Self::lanes_op(ty, [I::I8x16MaxU, I::I16x8MaxU, I::I32x4MaxU, I::Nop, I::Nop, I::Nop]),
                    });
                }
            }
        }
        Ok(())
    }

    fn vec_cmp(&mut self, cond: IntCond, a: Value, b: Value) {
        let ty = self.ty(a);
        let flip = ty == Type::V2I64 && matches!(cond, IntCond::Ult | IntCond::Ule | IntCond::Ugt | IntCond::Uge);
        self.get(a);
        if flip {
            self.vsplat_const(ty, 1 << 63);
            self.i(I::V128Xor);
        }
        self.get(b);
        if flip {
            self.vsplat_const(ty, 1 << 63);
            self.i(I::V128Xor);
        }
        let cond = if flip {
            match cond {
                IntCond::Ult => IntCond::Slt,
                IntCond::Ule => IntCond::Sle,
                IntCond::Ugt => IntCond::Sgt,
                _ => IntCond::Sge,
            }
        } else {
            cond
        };
        self.i(match cond {
            IntCond::Eq => Self::lanes_op(ty, [I::I8x16Eq, I::I16x8Eq, I::I32x4Eq, I::I64x2Eq, I::Nop, I::Nop]),
            IntCond::Ne => Self::lanes_op(ty, [I::I8x16Ne, I::I16x8Ne, I::I32x4Ne, I::I64x2Ne, I::Nop, I::Nop]),
            IntCond::Slt => Self::lanes_op(ty, [I::I8x16LtS, I::I16x8LtS, I::I32x4LtS, I::I64x2LtS, I::Nop, I::Nop]),
            IntCond::Sle => Self::lanes_op(ty, [I::I8x16LeS, I::I16x8LeS, I::I32x4LeS, I::I64x2LeS, I::Nop, I::Nop]),
            IntCond::Sgt => Self::lanes_op(ty, [I::I8x16GtS, I::I16x8GtS, I::I32x4GtS, I::I64x2GtS, I::Nop, I::Nop]),
            IntCond::Sge => Self::lanes_op(ty, [I::I8x16GeS, I::I16x8GeS, I::I32x4GeS, I::I64x2GeS, I::Nop, I::Nop]),
            IntCond::Ult => Self::lanes_op(ty, [I::I8x16LtU, I::I16x8LtU, I::I32x4LtU, I::Nop, I::Nop, I::Nop]),
            IntCond::Ule => Self::lanes_op(ty, [I::I8x16LeU, I::I16x8LeU, I::I32x4LeU, I::Nop, I::Nop, I::Nop]),
            IntCond::Ugt => Self::lanes_op(ty, [I::I8x16GtU, I::I16x8GtU, I::I32x4GtU, I::Nop, I::Nop, I::Nop]),
            IntCond::Uge => Self::lanes_op(ty, [I::I8x16GeU, I::I16x8GeU, I::I32x4GeU, I::Nop, I::Nop, I::Nop]),
        });
    }

    fn vec_reduce_add(&mut self, a: Value) {
        let ty = self.ty(a);
        self.get(a);
        match ty {
            Type::V2F64 => {
                self.i(I::LocalTee(self.tv[0]));
                self.i(I::F64x2ExtractLane(0));
                self.i(I::LocalGet(self.tv[0]));
                self.i(I::F64x2ExtractLane(1));
                self.i(I::F64Add);
            }
            Type::V4F32 => {
                self.i(I::LocalTee(self.tv[0]));
                self.i(I::F32x4ExtractLane(0));
                self.i(I::LocalGet(self.tv[0]));
                self.i(I::F32x4ExtractLane(1));
                self.i(I::F32Add);
                self.i(I::LocalGet(self.tv[0]));
                self.i(I::F32x4ExtractLane(2));
                self.i(I::LocalGet(self.tv[0]));
                self.i(I::F32x4ExtractLane(3));
                self.i(I::F32Add);
                self.i(I::F32Add);
            }
            _ => {
                let mut t = ty;
                if t == Type::V16I8 {
                    self.i(I::I16x8ExtAddPairwiseI8x16S);
                    t = Type::V8I16;
                }
                if t == Type::V8I16 {
                    self.i(I::I32x4ExtAddPairwiseI16x8S);
                    t = Type::V4I32;
                }
                if t == Type::V4I32 {
                    self.i(I::LocalTee(self.tv[0]));
                    self.i(I::I64x2ExtendLowI32x4S);
                    self.i(I::LocalGet(self.tv[0]));
                    self.i(I::I64x2ExtendHighI32x4S);
                    self.i(I::I64x2Add);
                }
                self.i(I::LocalTee(self.tv[0]));
                self.i(I::I64x2ExtractLane(0));
                self.i(I::LocalGet(self.tv[0]));
                self.i(I::I64x2ExtractLane(1));
                self.i(I::I64Add);
            }
        }
    }

    fn vec_narrow(&mut self, a: Value, b: Value) {
        let ty = self.ty(a);
        match ty {
            Type::V2I64 => {
                self.get(a);
                self.get(b);
                self.i(I::I8x16Shuffle([0, 1, 2, 3, 8, 9, 10, 11, 16, 17, 18, 19, 24, 25, 26, 27]));
            }
            _ => {
                let mask = if ty == Type::V8I16 { 0xff } else { 0xffff };
                self.get(a);
                self.vsplat_const(ty, mask);
                self.i(I::V128And);
                self.get(b);
                self.vsplat_const(ty, mask);
                self.i(I::V128And);
                self.i(if ty == Type::V8I16 { I::I8x16NarrowI16x8U } else { I::I16x8NarrowI32x4U });
            }
        }
    }

    // ---------------------------------------------------------------- dispatch

    fn inst(&mut self, block: Block, inst: &Instruction) -> Result<(), Refusal> {
        match &inst.op {
            Op::Const { ty, bits } => self.konst(*ty, *bits),
            Op::IntBin { op, a, b } => self.int_bin(*op, *a, *b),
            Op::IntOvf { op, a, b } => self.int_ovf(*op, *a, *b),
            Op::IntUn { op, a } => self.int_un(*op, *a),
            Op::Icmp { cond, a, b } => self.icmp(*cond, *a, *b),
            Op::FloatBin { op, a, b } => self.float_bin(*op, *a, *b),
            Op::FloatUn { op, a } => self.float_un(*op, *a),
            Op::Fcmp { cond, a, b } => self.fcmp(*cond, *a, *b),
            Op::Conv { op, to, a } => self.conv(*op, *to, *a),
            Op::Select { cond, a, b } => {
                self.get(*a);
                self.get(*b);
                self.get(*cond);
                self.i(I::Select);
            }
            Op::Load { ty, ptr, offset, unaligned } => self.load(*ty, *ptr, *offset, *unaligned, false),
            Op::LoadZext { ty, ptr, offset, unaligned } => self.load(*ty, *ptr, *offset, *unaligned, true),
            Op::Store { ptr, offset, value, unaligned } => self.store(*ptr, *offset, *value, *unaligned),
            Op::StackAddr { slot } => {
                self.i(I::LocalGet(self.fp));
                self.i(I::I32Const(self.offsets[slot.index()] as i32));
                self.i(I::I32Add);
                self.i(I::I64ExtendI32U);
            }
            Op::PtrAdd { base, index, scale, offset } => {
                self.get(*base);
                if *scale != 0 {
                    self.get(*index);
                    if *scale != 1 {
                        self.i(I::I64Const(*scale as i64));
                        self.i(I::I64Mul);
                    }
                    self.i(I::I64Add);
                }
                if *offset != 0 {
                    self.i(I::I64Const(*offset as i64));
                    self.i(I::I64Add);
                }
            }
            Op::Call { callee, args } => {
                for &a in args.iter() {
                    self.get(a);
                }
                self.i(I::Call(self.link.base + callee.0));
                let nret = self.program.func(*callee).returns.len();
                let keep = inst.result.is_some() as usize;
                for _ in keep..nret {
                    self.i(I::Drop);
                }
            }
            Op::CallHost { callee, args } => {
                let decl = self.program.host_fn(*callee);
                for &a in args.iter() {
                    self.get(a);
                }
                match self.link.hosts.get(callee.index()) {
                    // import mode: the host is the module's own function import (`Link`), a direct call
                    Some(&k) => self.i(I::Call(k)),
                    None => {
                        let (p, r) = host_sig(decl);
                        let type_index = self.types.index(&p, &r);
                        self.i(I::I32Const(decl.addr as u32 as i32));
                        self.i(I::CallIndirect { type_index, table_index: 0 });
                    }
                }
                if let Some(t) = decl.ret {
                    self.canon(t);
                }
            }
            Op::Prefetch { .. } => {}
            Op::Crc32c { acc, data } => {
                self.get(*acc);
                self.get(*data);
                let h = self.helper_index(Helper::Crc32c);
                self.i(I::Call(h));
            }
            Op::AddHi { alo, blo, ahi, bhi } => {
                // ahi + bhi + (alo + blo <u alo)
                self.get(*ahi);
                self.get(*bhi);
                self.i(I::I64Add);
                self.get(*alo);
                self.get(*blo);
                self.i(I::I64Add);
                self.get(*alo);
                self.i(I::I64LtU);
                self.i(I::I64ExtendI32U);
                self.i(I::I64Add);
            }
            Op::VecLoad { ty, ptr, offset, unaligned } => self.load(*ty, *ptr, *offset, *unaligned, false),
            Op::VecStore { ptr, offset, value, unaligned } => self.store(*ptr, *offset, *value, *unaligned),
            Op::VecBin { op, a, b } => self.vec_bin(block, *op, *a, *b)?,
            Op::VecShift { op, a, count } => {
                let ty = self.ty(*a);
                self.get(*a);
                self.i(I::I32Const(*count as i32));
                self.i(match op {
                    VecShiftOp::Shl => Self::lanes_op(ty, [I::I8x16Shl, I::I16x8Shl, I::I32x4Shl, I::I64x2Shl, I::Nop, I::Nop]),
                    VecShiftOp::Ushr => Self::lanes_op(ty, [I::I8x16ShrU, I::I16x8ShrU, I::I32x4ShrU, I::I64x2ShrU, I::Nop, I::Nop]),
                    VecShiftOp::Sshr => Self::lanes_op(ty, [I::I8x16ShrS, I::I16x8ShrS, I::I32x4ShrS, I::I64x2ShrS, I::Nop, I::Nop]),
                });
            }
            Op::VecCmp { cond, a, b } => self.vec_cmp(*cond, *a, *b),
            Op::VecSelect { mask, a, b } => {
                self.get(*a);
                self.get(*b);
                self.get(*mask);
                self.i(I::V128Bitselect);
            }
            Op::VecReduceAdd { a } => self.vec_reduce_add(*a),
            Op::VecMovmask { a } => {
                let ty = self.ty(*a);
                self.get(*a);
                self.i(Self::lanes_op(ty, [I::I8x16Bitmask, I::I16x8Bitmask, I::I32x4Bitmask, I::I64x2Bitmask, I::I32x4Bitmask, I::I64x2Bitmask]));
                self.i(I::I64ExtendI32U);
            }
            Op::VecSplat { ty, a } => {
                self.get(*a);
                self.i(Self::lanes_op(*ty, [I::I8x16Splat, I::I16x8Splat, I::I32x4Splat, I::I64x2Splat, I::F32x4Splat, I::F64x2Splat]));
            }
            Op::VecExtract { a, lane } => {
                let ty = self.ty(*a);
                self.get(*a);
                self.i(Self::lanes_op(ty, [I::I8x16ExtractLaneS(*lane), I::I16x8ExtractLaneS(*lane), I::I32x4ExtractLane(*lane), I::I64x2ExtractLane(*lane), I::F32x4ExtractLane(*lane), I::F64x2ExtractLane(*lane)]));
            }
            Op::VecInsert { a, lane, value } => {
                let ty = self.ty(*a);
                self.get(*a);
                self.get(*value);
                self.i(Self::lanes_op(ty, [I::I8x16ReplaceLane(*lane), I::I16x8ReplaceLane(*lane), I::I32x4ReplaceLane(*lane), I::I64x2ReplaceLane(*lane), I::F32x4ReplaceLane(*lane), I::F64x2ReplaceLane(*lane)]));
            }
            Op::VecWiden { a, signed, high } => {
                let ty = self.ty(*a);
                self.get(*a);
                self.i(match (ty, *signed, *high) {
                    (Type::V16I8, true, false) => I::I16x8ExtendLowI8x16S,
                    (Type::V16I8, true, true) => I::I16x8ExtendHighI8x16S,
                    (Type::V16I8, false, false) => I::I16x8ExtendLowI8x16U,
                    (Type::V16I8, false, true) => I::I16x8ExtendHighI8x16U,
                    (Type::V8I16, true, false) => I::I32x4ExtendLowI16x8S,
                    (Type::V8I16, true, true) => I::I32x4ExtendHighI16x8S,
                    (Type::V8I16, false, false) => I::I32x4ExtendLowI16x8U,
                    (Type::V8I16, false, true) => I::I32x4ExtendHighI16x8U,
                    (_, true, false) => I::I64x2ExtendLowI32x4S,
                    (_, true, true) => I::I64x2ExtendHighI32x4S,
                    (_, false, false) => I::I64x2ExtendLowI32x4U,
                    (_, false, true) => I::I64x2ExtendHighI32x4U,
                });
            }
            Op::VecNarrow { a, b } => self.vec_narrow(*a, *b),
        }
        if let Some(r) = inst.result {
            self.set(r);
        }
        Ok(())
    }
}

/// The out-of-line routines: signed 64-bit multiply overflow via the
/// 128-bit high word, and CRC-32C bit by bit (the interpreter's reference).
pub(crate) fn helper(h: Helper) -> ((Vec<ValType>, Vec<ValType>), WasmFunction) {
    match h {
        Helper::SmulOvf64 | Helper::Smulh64 | Helper::Umulh64 => {
            // locals: 0 a, 1 b, 2 a0, 3 a1, 4 b0, 5 b1, 6 p10, 7 mid, 8 hi
            let mut w = WasmFunction::new([(7, ValType::I64)]);
            let ins = |w: &mut WasmFunction, i: I<'_>| {
                w.instruction(&i);
            };
            let lo = |w: &mut WasmFunction, src: u32, dst: u32| {
                ins(w, I::LocalGet(src));
                ins(w, I::I64Const(0xffff_ffff));
                ins(w, I::I64And);
                ins(w, I::LocalSet(dst));
            };
            let hi = |w: &mut WasmFunction, src: u32, dst: u32| {
                ins(w, I::LocalGet(src));
                ins(w, I::I64Const(32));
                ins(w, I::I64ShrU);
                ins(w, I::LocalSet(dst));
            };
            lo(&mut w, 0, 2);
            hi(&mut w, 0, 3);
            lo(&mut w, 1, 4);
            hi(&mut w, 1, 5);
            // p10 = a1*b0
            ins(&mut w, I::LocalGet(3));
            ins(&mut w, I::LocalGet(4));
            ins(&mut w, I::I64Mul);
            ins(&mut w, I::LocalSet(6));
            // mid = (a0*b0 >> 32) + (p10 & mask) + a0*b1
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::LocalGet(4));
            ins(&mut w, I::I64Mul);
            ins(&mut w, I::I64Const(32));
            ins(&mut w, I::I64ShrU);
            ins(&mut w, I::LocalGet(6));
            ins(&mut w, I::I64Const(0xffff_ffff));
            ins(&mut w, I::I64And);
            ins(&mut w, I::I64Add);
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::LocalGet(5));
            ins(&mut w, I::I64Mul);
            ins(&mut w, I::I64Add);
            ins(&mut w, I::LocalSet(7));
            // hi = a1*b1 + (p10 >> 32) + (mid >> 32) - (a<0 ? b : 0) - (b<0 ? a : 0)
            ins(&mut w, I::LocalGet(3));
            ins(&mut w, I::LocalGet(5));
            ins(&mut w, I::I64Mul);
            ins(&mut w, I::LocalGet(6));
            ins(&mut w, I::I64Const(32));
            ins(&mut w, I::I64ShrU);
            ins(&mut w, I::I64Add);
            ins(&mut w, I::LocalGet(7));
            ins(&mut w, I::I64Const(32));
            ins(&mut w, I::I64ShrU);
            ins(&mut w, I::I64Add);
            if h == Helper::Umulh64 {
                // the unsigned high word: no sign corrections
                ins(&mut w, I::End);
                return ((vec![ValType::I64, ValType::I64], vec![ValType::I64]), w);
            }
            ins(&mut w, I::LocalGet(1));
            ins(&mut w, I::LocalGet(0));
            ins(&mut w, I::I64Const(63));
            ins(&mut w, I::I64ShrS);
            ins(&mut w, I::I64And);
            ins(&mut w, I::I64Sub);
            ins(&mut w, I::LocalGet(0));
            ins(&mut w, I::LocalGet(1));
            ins(&mut w, I::I64Const(63));
            ins(&mut w, I::I64ShrS);
            ins(&mut w, I::I64And);
            ins(&mut w, I::I64Sub);
            if h == Helper::Smulh64 {
                ins(&mut w, I::End);
                return ((vec![ValType::I64, ValType::I64], vec![ValType::I64]), w);
            }
            // != (a*b) >> 63
            ins(&mut w, I::LocalGet(0));
            ins(&mut w, I::LocalGet(1));
            ins(&mut w, I::I64Mul);
            ins(&mut w, I::I64Const(63));
            ins(&mut w, I::I64ShrS);
            ins(&mut w, I::I64Ne);
            ins(&mut w, I::End);
            ((vec![ValType::I64, ValType::I64], vec![ValType::I32]), w)
        }
        Helper::Crc32c => {
            // locals: 0 acc, 1 data, 2 crc, 3 i, 4 j
            let mut w = WasmFunction::new([(3, ValType::I32)]);
            let ins = |w: &mut WasmFunction, i: I<'_>| {
                w.instruction(&i);
            };
            ins(&mut w, I::LocalGet(0));
            ins(&mut w, I::I32WrapI64);
            ins(&mut w, I::LocalSet(2));
            ins(&mut w, I::Loop(BlockType::Empty));
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::LocalGet(1));
            ins(&mut w, I::LocalGet(3));
            ins(&mut w, I::I32Const(3));
            ins(&mut w, I::I32Shl);
            ins(&mut w, I::I64ExtendI32U);
            ins(&mut w, I::I64ShrU);
            ins(&mut w, I::I32WrapI64);
            ins(&mut w, I::I32Const(0xff));
            ins(&mut w, I::I32And);
            ins(&mut w, I::I32Xor);
            ins(&mut w, I::LocalSet(2));
            ins(&mut w, I::I32Const(0));
            ins(&mut w, I::LocalSet(4));
            ins(&mut w, I::Loop(BlockType::Empty));
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::I32Const(1));
            ins(&mut w, I::I32ShrU);
            ins(&mut w, I::I32Const(0x82F6_3B78u32 as i32));
            ins(&mut w, I::I32Const(0));
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::I32Const(1));
            ins(&mut w, I::I32And);
            ins(&mut w, I::I32Sub);
            ins(&mut w, I::I32And);
            ins(&mut w, I::I32Xor);
            ins(&mut w, I::LocalSet(2));
            ins(&mut w, I::LocalGet(4));
            ins(&mut w, I::I32Const(1));
            ins(&mut w, I::I32Add);
            ins(&mut w, I::LocalTee(4));
            ins(&mut w, I::I32Const(8));
            ins(&mut w, I::I32LtU);
            ins(&mut w, I::BrIf(0));
            ins(&mut w, I::End);
            ins(&mut w, I::LocalGet(3));
            ins(&mut w, I::I32Const(1));
            ins(&mut w, I::I32Add);
            ins(&mut w, I::LocalTee(3));
            ins(&mut w, I::I32Const(8));
            ins(&mut w, I::I32LtU);
            ins(&mut w, I::BrIf(0));
            ins(&mut w, I::End);
            ins(&mut w, I::LocalGet(2));
            ins(&mut w, I::I64ExtendI32U);
            ins(&mut w, I::End);
            ((vec![ValType::I64, ValType::I64], vec![ValType::I64]), w)
        }
    }
}
