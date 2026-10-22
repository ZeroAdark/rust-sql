//! `X64Frontend` for Gravity IR: one `compile_inst` per Gravity op, the
//! x86-64 twin of `frontend.rs`.
//!
//! Register convention (matches `gravity_interp::canon` and the AArch64
//! frontend): every integer or pointer value lives in a 64-bit GP register,
//! narrow integers sign-extended, `Bool` as 0/1. Floats live in XMM
//! registers (low 32 bits for F32, low 64 for F64). Calls follow System V.
//!
//! Where x86 differs from AArch64 (see `docs/design/gravity-simple/x64-backend.md`):
//! two-address arithmetic (`mov` then `op`), `idiv`/`div` through RAX:RDX,
//! variable shifts through CL, overflow predicates from OF, `minsd`/`maxsd`
//! not being IEEE minNum (fixed up with `ucomisd` + `cmovp`), `ucomisd`
//! parity for NaN-aware compares, `bsr`/`bsf` being undefined on zero
//! (fixed up with `cmov`), `roundsd` (SSE4.1) for the rounding ops, and the
//! unsigned <-> float conversions above 2^63 needing explicit branches.

use crate::adaptor::{GBlock, GFunc, GInst, GValue, GravityAdaptor};
use crate::view::VInst;
use alloc::vec::Vec;
use gravity_ir::{ConvOp, FloatBinOp, FloatCond, FloatUnOp, IntBinOp, IntCond, IntOvfOp, IntUnOp, Op, Terminator, Type, VecBinOp, VecShiftOp};
use tpde_assembler::SymRef;
use tpde_compiler::{
    branches,
    calls::{CallArg, CallTarget, RetBuilder},
    target::{CompilerOutput, CompilerTarget, TargetMachine},
    RegallocEvent, ScratchReg, ValuePartRef,
};
use tpde_compiler_x64::{
    calls,
    emission::{self, gp, xmm},
    emit, encoding_error, regs, Jump, X64CompilerState, X64Frontend, CPU_POPCNT, CPU_SSE4_1, CPU_SSE4_2,
};
use tpde_core::{
    compiler::{ValRefSpecial, ValueParts},
    RegBank, Reg,
};
use tpde_fadec::*;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

type A<'p> = GravityAdaptor<'p>;

pub const GP_BANK: RegBank = RegBank::new(0);
pub const FP_BANK: RegBank = RegBank::new(1);

#[derive(Clone, Copy, Debug)]
pub struct Parts {
    size: u32,
    bank: RegBank,
}
impl ValueParts for Parts {
    fn count(&self) -> u32 {
        1
    }
    fn size_bytes(&self, _: u32) -> u32 {
        self.size
    }
    fn reg_bank(&self, _: u32) -> RegBank {
        self.bank
    }
}

fn parts_of(ty: Type) -> Parts {
    match ty {
        Type::F32 => Parts { size: 4, bank: FP_BANK },
        Type::F64 => Parts { size: 8, bank: FP_BANK },
        // A 128-bit vector is one XMM register (16-byte spill slot).
        t if t.is_vector() => Parts { size: 16, bank: FP_BANK },
        _ => Parts { size: 8, bank: GP_BANK },
    }
}

#[derive(Debug, Default)]
pub struct GravityFrontendX64 {
    /// See `frontend::GravityFrontend::events` and `record`.
    pub events: core::cell::RefCell<Vec<crate::RegEvent>>,
    pub record: bool,
    /// Nanoseconds in TPDE's analyzer (`analysis_start` .. `analysis_end`)
    /// over every function (`crate::CompileTiming::analysis_ns`).
    pub analysis_ns: core::cell::Cell<u64>,
    analysis_t0: core::cell::Cell<u64>,
}

impl GravityFrontendX64 {
    /// See `frontend::GravityFrontend::with_record`.
    pub fn with_record(record: bool) -> Self {
        GravityFrontendX64 { record, ..Default::default() }
    }
}

/// x86-64 immediate rules: any sign-extended imm32 for add/sub/and/or/xor
/// and cmp (also when the compare is fused into the branch); no zero
/// register, so `Select` never folds.
pub struct X64Imm;
pub static X64_IMM: X64Imm = X64Imm;
impl crate::view::ImmRules for X64Imm {
    fn add_imm(&self, k: i64) -> bool {
        k as i32 as i64 == k
    }
    fn logical_imm(&self, k: i64) -> bool {
        k as i32 as i64 == k
    }
    fn cmp_imm(&self, k: i64) -> bool {
        k as i32 as i64 == k
    }
    fn branch_cmp_imm(&self, k: i64) -> bool {
        k as i32 as i64 == k
    }
    fn select_zero(&self) -> bool {
        false
    }
    fn phi_const(&self) -> bool {
        true
    }
}

impl<'p> X64Frontend<A<'p>> for GravityFrontendX64 {
    type Special = ValRefSpecial<()>;
    type Parts = Parts;

    fn val_parts(&self, adaptor: &A<'p>, value: GValue) -> Parts {
        parts_of(adaptor.view.value_type(adaptor.func(), value.0))
    }
    fn val_ref_special(&self, _: &A<'p>, _: GValue) -> Option<Self::Special> {
        None
    }
    fn analysis_start(&mut self, _: &A<'p>) {
        self.analysis_t0.set(crate::now_ns());
    }
    fn analysis_end(&mut self, _: &A<'p>) {
        self.analysis_ns.set(self.analysis_ns.get() + (crate::now_ns() - self.analysis_t0.get()));
    }
    fn cur_func_may_emit_calls(&self, adaptor: &A<'p>) -> bool {
        adaptor.view.has_calls
    }
    fn cur_personality_func(&self, _: &A<'p>) -> SymRef {
        SymRef::default()
    }
    fn define_func_idx(&mut self, _: &A<'p>, function: GFunc, index: u32) {
        assert_eq!(function.0, index);
    }
    fn try_force_fixed_assignment(&self, adaptor: &A<'p>, value: GValue) -> bool {
        adaptor.view.force_fixed.get(value.0 as usize).copied().unwrap_or(false)
    }
    const PHI_COALESCING: bool = true;
    const WEIGHTED_REGALLOC: bool = false;
    const PINNED_ASSIGNMENTS: [u32; 2] = [3, 3];
    fn value_weight(&self, adaptor: &A<'p>, value: GValue) -> Option<u32> {
        adaptor.view.weight.get(value.0 as usize).copied()
    }
    fn regalloc_event(&self, adaptor: &A<'p>, event: RegallocEvent, block: GBlock) {
        if self.record {
            self.events.borrow_mut().push(crate::RegEvent::new(adaptor, event, block));
        }
    }
    fn allow_fixed_assignment(&self, adaptor: &A<'p>, _: GValue) -> bool {
        // Same rule as the AArch64 frontend: multi-value returns must not
        // pin values to argument registers.
        adaptor.func().returns.len() <= 1
    }
    fn special_part<'s, 'm, M>(_: &'s M, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, M, 2>
    where
        A<'p>: 's,
        M: TargetMachine<'m, A<'p>, 2>,
        M::Target: X64CompilerState<Frontend = Self> + CompilerTarget<A<'p>, 2, Special = Self::Special>,
    {
        unreachable!("gravity has no special values")
    }
    fn compile_inst<'m, M>(machine: &M, inst: GInst, _: impl Iterator<Item = GInst> + Clone) -> PgResult<bool>
    where
        M: TargetMachine<'m, A<'p>, 2>,
        M::Target: X64CompilerState<Frontend = Self> + CompilerTarget<A<'p>, 2, Special = Self::Special, Jump = Jump>,
    {
        let adaptor = machine.adaptor();
        if let (Some(t), Some(j)) = (adaptor.jit_tag(inst), &adaptor.jit) {
            j.at(machine.with_output(|o| o.offset()), t); // the JIT map's (crate::compile_tagged): offsets only
        }
        match adaptor.inst(inst) {
            VInst::Alloca { .. } => Ok(true),
            VInst::SplitBr { split } => {
                let target = adaptor.view.splits[split as usize].target;
                branches::generate_uncond_branch(machine, GBlock(target))?;
                Ok(true)
            }
            VInst::Term { block } => compile_term(machine, block),
            VInst::Op { block, idx } => {
                let func = adaptor.func();
                let i = &func.blocks[block as usize].insts[idx as usize];
                let r = adaptor.view.results[inst.0 as usize];
                let result = (r != u32::MAX).then_some(GValue(r));
                compile_op(machine, inst, &i.op, result)
            }
        }
    }
}

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message).with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE).into()
}

macro_rules! insn {
    ($m:expr, $i:ident $(, $o:expr)* $(,)?) => {
        emit($m, |out| $i(out, 0 $(, $o)*))
    };
}

fn ty_of<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, v: GValue) -> Type {
    let a = machine.adaptor();
    a.view.value_type(a.func(), v.0)
}

fn bits(ty: Type) -> u32 {
    ty.bytes() * 8
}

fn mem(base: Reg, off: i32) -> PgResult<FeMem> {
    FE_MEM(gp(base), 0, FE_NOREG, off).map_err(encoding_error)
}

fn has_feat<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, feat: u32) -> bool
where
    M::Target: X64CompilerState,
{
    machine.with_target(|t| t.x64().has_cpu_feats(feat))
}

/// Sign-extend `reg` in place to the canonical form of `ty`.
fn canon<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, reg: Reg, ty: Type) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let r = gp(reg);
    match ty {
        Type::I8 => insn!(machine, fe64_MOVSXr64r8, r, r),
        Type::I16 => insn!(machine, fe64_MOVSXr64r16, r, r),
        Type::I32 => insn!(machine, fe64_MOVSXr64r32, r, r),
        Type::Bool => insn!(machine, fe64_MOVZXr32r8, r, r),
        _ => Ok(()),
    }
}

/// Zero-extend `src` (canonical `ty`) into `dst`.
fn zext_into<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg, ty: Type) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let (d, s) = (gp(dst), gp(src));
    match ty {
        Type::I8 | Type::Bool => insn!(machine, fe64_MOVZXr32r8, d, s),
        Type::I16 => insn!(machine, fe64_MOVZXr32r16, d, s),
        Type::I32 => insn!(machine, fe64_MOV32rr, d, s),
        _ => mov_gp(machine, dst, src),
    }
}

fn mov_gp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if dst != src {
        insn!(machine, fe64_MOV64rr, gp(dst), gp(src))
    } else {
        Ok(())
    }
}

fn mov_fp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if dst != src {
        insn!(machine, fe64_SSE_MOVAPDrr, xmm(dst), xmm(src))
    } else {
        Ok(())
    }
}

/// `mov64 dst, imm` without the flag-clobbering `xor` shortcut.
fn mov_imm<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, imm: u64) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if imm <= u32::MAX as u64 {
        insn!(machine, fe64_MOV32ri, gp(dst), imm as u32 as i32)
    } else {
        insn!(machine, fe64_MOV64ri, gp(dst), imm as i64)
    }
}

/// GP -> XMM move of `size` bytes (4 or 8).
fn gp_to_fp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg, size: u32) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if size == 4 {
        insn!(machine, fe64_SSE_MOVD_G2Xrr, xmm(dst), gp(src))
    } else {
        insn!(machine, fe64_SSE_MOVQ_G2Xrr, xmm(dst), gp(src))
    }
}

/// XMM -> GP move of `size` bytes (4 or 8); the GP upper bits are zero.
fn fp_to_gp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg, size: u32) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if size == 4 {
        insn!(machine, fe64_SSE_MOVD_X2Grr, gp(dst), xmm(src))
    } else {
        insn!(machine, fe64_SSE_MOVQ_X2Grr, gp(dst), xmm(src))
    }
}

fn icond(c: IntCond) -> Jump {
    match c {
        IntCond::Eq => Jump::je,
        IntCond::Ne => Jump::jne,
        IntCond::Slt => Jump::jl,
        IntCond::Sle => Jump::jle,
        IntCond::Sgt => Jump::jg,
        IntCond::Sge => Jump::jge,
        IntCond::Ult => Jump::jb,
        IntCond::Ule => Jump::jbe,
        IntCond::Ugt => Jump::ja,
        IntCond::Uge => Jump::jae,
    }
}

/// `setcc` into `dst` with the upper bits cleared (flags preserved).
fn set_cc<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, cc: Jump, dst: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    emission::generate_raw_set(machine, cc, dst, true)
}

fn cmov<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, cc: Jump, dst: Reg, src: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    emission::generate_raw_cmov(machine, cc, dst, src, true)
}

/// Unary op on one operand producing a result in a fresh (or reused) register.
fn unary<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let a_ref = machine.value_ref(a)?;
    let mut ap = a_ref.part(0);
    let r_ref = machine.result_ref(result)?;
    let mut rp = r_ref.part(0);
    let a_reg = ap.load_to_reg()?;
    let r_reg = rp.alloc_try_reuse(&mut ap)?;
    emit(machine, r_reg, a_reg)?;
    rp.set_modified();
    Ok(())
}

/// Unary op whose result bank differs from the operand bank (never reuse).
fn unary_cross<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let a_ref = machine.value_ref(a)?;
    let mut ap = a_ref.part(0);
    let r_ref = machine.result_ref(result)?;
    let mut rp = r_ref.part(0);
    let a_reg = ap.load_to_reg()?;
    let r_reg = rp.alloc_reg()?;
    emit(machine, r_reg, a_reg)?;
    rp.set_modified();
    Ok(())
}

/// Binary op; the result may reuse the first operand's register.
fn binary<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, b: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let a_ref = machine.value_ref(a)?;
    let mut ap = a_ref.part(0);
    let b_ref = machine.value_ref(b)?;
    let mut bp = b_ref.part(0);
    let r_ref = machine.result_ref(result)?;
    let mut rp = r_ref.part(0);
    let a_reg = ap.load_to_reg()?;
    let b_reg = bp.load_to_reg()?;
    let r_reg = rp.alloc_try_reuse(&mut ap)?;
    emit(machine, r_reg, a_reg, b_reg)?;
    rp.set_modified();
    Ok(())
}

/// Binary op whose result must not alias an operand.
fn binary_fresh<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, b: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let a_ref = machine.value_ref(a)?;
    let mut ap = a_ref.part(0);
    let b_ref = machine.value_ref(b)?;
    let mut bp = b_ref.part(0);
    let r_ref = machine.result_ref(result)?;
    let mut rp = r_ref.part(0);
    let a_reg = ap.load_to_reg()?;
    let b_reg = bp.load_to_reg()?;
    let r_reg = rp.alloc_reg()?;
    emit(machine, r_reg, a_reg, b_reg)?;
    rp.set_modified();
    Ok(())
}

/// Two-address GP arithmetic: `rd = rn OP rm` through `mov rd, rn; op rd, rm`.
fn two_addr_gp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, rd: Reg, rn: Reg, rm: Reg, commutative: bool, op: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if rd == rn {
        op(machine, rd, rm)
    } else if rd == rm {
        if commutative {
            op(machine, rd, rn)
        } else {
            let mut tmp = ScratchReg::new(machine);
            let t = tmp.alloc_gp()?;
            mov_gp(machine, t, rm)?;
            mov_gp(machine, rd, rn)?;
            op(machine, rd, t)
        }
    } else {
        mov_gp(machine, rd, rn)?;
        op(machine, rd, rm)
    }
}

/// Two-address XMM arithmetic, same shape as `two_addr_gp`.
fn two_addr_fp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, rd: Reg, rn: Reg, rm: Reg, commutative: bool, op: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if rd == rn {
        op(machine, rd, rm)
    } else if rd == rm {
        if commutative {
            op(machine, rd, rn)
        } else {
            let mut tmp = ScratchReg::new(machine);
            let t = tmp.alloc(FP_BANK)?;
            mov_fp(machine, t, rm)?;
            mov_fp(machine, rd, rn)?;
            op(machine, rd, t)
        }
    } else {
        mov_fp(machine, rd, rn)?;
        op(machine, rd, rm)
    }
}

fn compile_op<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, inst: GInst, op: &Op, result: Option<GValue>) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + X64CompilerState<Frontend = GravityFrontendX64>,
{
    let g = |v: gravity_ir::Value| GValue(v.0);
    match *op {
        Op::Const { ty, bits } => {
            let r = result.unwrap();
            let r_ref = machine.result_ref(r)?;
            let mut rp = r_ref.part(0);
            let reg = rp.alloc_reg()?;
            match ty {
                Type::F64 | Type::F32 => {
                    let size = ty.bytes();
                    let mut tmp = ScratchReg::new(machine);
                    let t = tmp.alloc_gp()?;
                    mov_imm(machine, t, bits)?;
                    gp_to_fp(machine, reg, t, size)?;
                }
                _ => {
                    let v = gravity_interp_canon(ty, bits);
                    emission::materialize_constant(machine, &[v], GP_BANK, 8, reg)?;
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::IntBin { op: bop, a, b } => {
            let ty = ty_of(machine, g(a));
            // A no-wrap result already fits its type: canonicalizing as I64
            // is a no-op, so pass that as the canon type.
            let cty = if machine.adaptor().view.nowrap.get(result.unwrap().0 as usize).copied().unwrap_or(false) { Type::I64 } else { ty };
            if let Some((k, cv)) = folded_imm_operand(machine, op) {
                let w = bits(ty);
                // The register operand is whichever side is not the folded
                // constant (a commutative op may have it on the left).
                let src = if cv == b { a } else { b };
                unary(machine, g(src), result.unwrap(), |m, rd, rn| {
                    let d = gp(rd);
                    match bop {
                        IntBinOp::Ushr if w < 64 => zext_into(m, rd, rn, ty)?,
                        _ => mov_gp(m, rd, rn)?,
                    }
                    match bop {
                        IntBinOp::Add => insn!(m, fe64_ADD64ri, d, k as i32)?,
                        IntBinOp::Sub => insn!(m, fe64_SUB64ri, d, k as i32)?,
                        IntBinOp::And => insn!(m, fe64_AND64ri, d, k as i32)?,
                        IntBinOp::Or => insn!(m, fe64_OR64ri, d, k as i32)?,
                        IntBinOp::Xor => insn!(m, fe64_XOR64ri, d, k as i32)?,
                        IntBinOp::Shl => insn!(m, fe64_SHL64ri, d, k as i8)?,
                        IntBinOp::Ushr => insn!(m, fe64_SHR64ri, d, k as i8)?,
                        IntBinOp::Sshr => insn!(m, fe64_SAR64ri, d, k as i8)?,
                        IntBinOp::Rotl => insn!(m, fe64_ROL64ri, d, k as i8)?,
                        _ => unreachable!(),
                    }
                    canon(m, rd, cty)
                })?;
                return Ok(true);
            }
            compile_int_bin(machine, bop, ty, cty, g(a), g(b), result.unwrap())?;
            Ok(true)
        }
        Op::IntOvf { op, a, b } => {
            let ty = ty_of(machine, g(a));
            compile_int_ovf(machine, op, ty, g(a), g(b), result.unwrap())?;
            Ok(true)
        }
        Op::IntUn { op, a } => {
            let ty = ty_of(machine, g(a));
            compile_int_un(machine, op, ty, g(a), result.unwrap())?;
            Ok(true)
        }
        Op::Icmp { cond, a, b } => {
            if let Some(k) = folded_imm(machine, op) {
                unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                    insn!(m, fe64_CMP64ri, gp(rn), k as i32)?;
                    set_cc(m, icond(cond), rd)
                })?;
                return Ok(true);
            }
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                insn!(m, fe64_CMP64rr, gp(rn), gp(rm))?;
                set_cc(m, icond(cond), rd)
            })?;
            Ok(true)
        }
        Op::FloatBin { op, a, b } => {
            let ty = ty_of(machine, g(a));
            compile_float_bin(machine, op, ty, g(a), g(b), result.unwrap())?;
            Ok(true)
        }
        Op::FloatUn { op, a } => {
            let ty = ty_of(machine, g(a));
            compile_float_un(machine, op, ty, g(a), result.unwrap())?;
            Ok(true)
        }
        Op::Fcmp { cond, a, b } => {
            let d = ty_of(machine, g(a)) == Type::F64;
            // Result is GP, operands FP: allocate fresh.
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let b_ref = machine.value_ref(g(b))?;
            let mut bp = b_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let (ra, rb) = (ap.load_to_reg()?, bp.load_to_reg()?);
            let rd = rp.alloc_reg()?;
            let ucomis = |m: &M, x: Reg, y: Reg| -> PgResult<()> {
                if d {
                    insn!(m, fe64_SSE_UCOMISDrr, xmm(x), xmm(y))
                } else {
                    insn!(m, fe64_SSE_UCOMISSrr, xmm(x), xmm(y))
                }
            };
            // `ucomis` sets ZF/PF/CF = 1/1/1 on unordered; the relational
            // predicates use the unsigned conditions (CF/ZF) so NaN reads as
            // false, swapping operands for `<` and `<=`.
            match cond {
                FloatCond::Eq | FloatCond::Ne => {
                    ucomis(machine, ra, rb)?;
                    let mut tmp = ScratchReg::new(machine);
                    let t = tmp.alloc_gp()?;
                    if cond == FloatCond::Eq {
                        set_cc(machine, Jump::je, rd)?;
                        set_cc(machine, Jump::jnp, t)?;
                        insn!(machine, fe64_AND64rr, gp(rd), gp(t))?;
                    } else {
                        set_cc(machine, Jump::jne, rd)?;
                        set_cc(machine, Jump::jp, t)?;
                        insn!(machine, fe64_OR64rr, gp(rd), gp(t))?;
                    }
                }
                FloatCond::Lt => {
                    ucomis(machine, rb, ra)?;
                    set_cc(machine, Jump::ja, rd)?;
                }
                FloatCond::Le => {
                    ucomis(machine, rb, ra)?;
                    set_cc(machine, Jump::jae, rd)?;
                }
                FloatCond::Gt => {
                    ucomis(machine, ra, rb)?;
                    set_cc(machine, Jump::ja, rd)?;
                }
                FloatCond::Ge => {
                    ucomis(machine, ra, rb)?;
                    set_cc(machine, Jump::jae, rd)?;
                }
                FloatCond::Ordered => {
                    ucomis(machine, ra, rb)?;
                    set_cc(machine, Jump::jnp, rd)?;
                }
                FloatCond::Unordered => {
                    ucomis(machine, ra, rb)?;
                    set_cc(machine, Jump::jp, rd)?;
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::Conv { op, to, a } => {
            let from = ty_of(machine, g(a));
            if from.is_vector() || to.is_vector() {
                if op != ConvOp::Bitcast || !(from.is_vector() && to.is_vector()) {
                    return Err(invalid("gravity_tpde: a vector conversion other than a vector bitcast is not encoded"));
                }
                unary(machine, g(a), result.unwrap(), |m, rd, rn| mov_fp(m, rd, rn))?;
                return Ok(true);
            }
            compile_conv(machine, op, from, to, g(a), result.unwrap())?;
            Ok(true)
        }
        Op::Select { cond, a, b } => {
            let ty = ty_of(machine, g(a));
            if folded_imm(machine, op).is_some() {
                return Err(invalid("gravity_tpde: x86-64 never folds Select operands"));
            }
            let c_ref = machine.value_ref(g(cond))?;
            let mut cp = c_ref.part(0);
            let c = cp.load_to_reg()?;
            if ty.is_float() {
                let size = ty.bytes();
                binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                    let mut t1 = ScratchReg::new(m);
                    let mut t2 = ScratchReg::new(m);
                    let x = t1.alloc_gp()?;
                    let y = t2.alloc_gp()?;
                    fp_to_gp(m, x, rn, size)?;
                    fp_to_gp(m, y, rm, size)?;
                    insn!(m, fe64_TEST64rr, gp(c), gp(c))?;
                    cmov(m, Jump::je, x, y)?;
                    gp_to_fp(m, rd, x, size)
                })?;
            } else {
                binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                    // `rd` may alias `rn` (reuse) but never `rm`; every move
                    // before the `test` preserves the flags anyway.
                    mov_gp(m, rd, rn)?;
                    insn!(m, fe64_TEST64rr, gp(c), gp(c))?;
                    cmov(m, Jump::je, rd, rm)
                })?;
            }
            Ok(true)
        }
        Op::Load { ty, ptr, offset, .. } | Op::LoadZext { ty, ptr, offset, .. } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let base = pp.load_to_reg()?;
            let dst = rp.alloc_reg()?;
            let m = mem(base, offset)?;
            let zext = matches!(op, Op::LoadZext { .. }) || machine.adaptor().view.load_zext.get(inst.0 as usize).copied().unwrap_or(false);
            match ty {
                Type::I8 if zext => insn!(machine, fe64_MOVZXr32m8, gp(dst), m)?,
                Type::I16 if zext => insn!(machine, fe64_MOVZXr32m16, gp(dst), m)?,
                Type::I32 if zext => insn!(machine, fe64_MOV32rm, gp(dst), m)?,
                Type::Bool => insn!(machine, fe64_MOVZXr32m8, gp(dst), m)?,
                Type::I8 => insn!(machine, fe64_MOVSXr64m8, gp(dst), m)?,
                Type::I16 => insn!(machine, fe64_MOVSXr64m16, gp(dst), m)?,
                Type::I32 => insn!(machine, fe64_MOVSXr64m32, gp(dst), m)?,
                Type::I64 | Type::Ptr => insn!(machine, fe64_MOV64rm, gp(dst), m)?,
                Type::F32 => insn!(machine, fe64_SSE_MOVSSrm, xmm(dst), m)?,
                Type::F64 => insn!(machine, fe64_SSE_MOVSDrm, xmm(dst), m)?,
                _ => return Err(invalid("gravity_tpde: a vector load is `vload` (simd.md)")),
            }
            rp.set_modified();
            Ok(true)
        }
        Op::Store { ptr, offset, value, .. } => {
            let ty = ty_of(machine, g(value));
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let v_ref = machine.value_ref(g(value))?;
            let mut vp = v_ref.part(0);
            let base = pp.load_to_reg()?;
            let src = vp.load_to_reg()?;
            let m = mem(base, offset)?;
            match ty {
                Type::Bool | Type::I8 => insn!(machine, fe64_MOV8mr, m, gp(src))?,
                Type::I16 => insn!(machine, fe64_MOV16mr, m, gp(src))?,
                Type::I32 => insn!(machine, fe64_MOV32mr, m, gp(src))?,
                Type::I64 | Type::Ptr => insn!(machine, fe64_MOV64mr, m, gp(src))?,
                Type::F32 => insn!(machine, fe64_SSE_MOVSSmr, m, xmm(src))?,
                Type::F64 => insn!(machine, fe64_SSE_MOVSDmr, m, xmm(src))?,
                _ => return Err(invalid("gravity_tpde: a vector store is `vstore` (simd.md)")),
            }
            Ok(true)
        }
        Op::Prefetch { ptr, offset } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let base = pp.load_to_reg()?;
            let m = mem(base, offset)?;
            insn!(machine, fe64_PREFETCHT0m, m)?;
            Ok(true)
        }
        Op::AddHi { alo, blo, ahi, bhi } => {
            // `add t, blo` sets CF to the low word's carry; the moves before `adc` keep the flags.
            let l_ref = machine.value_ref(g(alo))?;
            let mut lp = l_ref.part(0);
            let m_ref = machine.value_ref(g(blo))?;
            let mut mp = m_ref.part(0);
            let (rl, rm) = (lp.load_to_reg()?, mp.load_to_reg()?);
            let mut st = ScratchReg::new(machine);
            let t = st.alloc_gp()?;
            binary(machine, g(ahi), g(bhi), result.unwrap(), |m, rd, rn, rr| {
                mov_gp(m, t, rl)?;
                insn!(m, fe64_ADD64rr, gp(t), gp(rm))?;
                // `rd` may alias `rn` (reuse) but never `rr`
                mov_gp(m, rd, rn)?;
                insn!(m, fe64_ADC64rr, gp(rd), gp(rr))
            })?;
            Ok(true)
        }
        Op::Crc32c { acc, data } => {
            // `crc32 r64, r64` accumulates in place: the result register
            // takes the accumulator first (it may reuse its register,
            // never the data's).
            binary(machine, g(acc), g(data), result.unwrap(), |m, rd, rn, rm| {
                mov_gp(m, rd, rn)?;
                insn!(m, fe64_CRC32_64rr, gp(rd), gp(rm))
            })?;
            Ok(true)
        }
        Op::StackAddr { slot } => {
            let nv = machine.adaptor().view.num_values;
            unary(machine, GValue(nv + slot.0), result.unwrap(), |m, rd, rn| mov_gp(m, rd, rn))?;
            Ok(true)
        }
        Op::PtrAdd { base, index, scale, offset } => {
            if let Some(k) = folded_imm(machine, op) {
                // Constant index: `lea rd, [rn + k * scale + offset]`.
                let total = k.wrapping_mul(i64::from(scale)).wrapping_add(i64::from(offset));
                unary(machine, g(base), result.unwrap(), |m, rd, rn| {
                    if total == 0 {
                        mov_gp(m, rd, rn)
                    } else {
                        let a = mem(rn, total as i32)?;
                        insn!(m, fe64_LEA64rm, gp(rd), a)
                    }
                })?;
                return Ok(true);
            }
            binary_fresh(machine, g(base), g(index), result.unwrap(), |m, rd, rn, rm| {
                if scale == 0 {
                    if offset != 0 {
                        let a = mem(rn, offset)?;
                        insn!(m, fe64_LEA64rm, gp(rd), a)
                    } else {
                        mov_gp(m, rd, rn)
                    }
                } else if matches!(scale, 1 | 2 | 4 | 8) {
                    let a = FE_MEM(gp(rn), scale, gp(rm), offset).map_err(encoding_error)?;
                    insn!(m, fe64_LEA64rm, gp(rd), a)
                } else {
                    insn!(m, fe64_IMUL64rri, gp(rd), gp(rm), i32::from(scale))?;
                    let a = FE_MEM(gp(rn), 1, gp(rd), offset).map_err(encoding_error)?;
                    insn!(m, fe64_LEA64rm, gp(rd), a)
                }
            })?;
            Ok(true)
        }
        Op::Call { callee, ref args } => {
            let sym = machine.function_symbol(callee.0);
            let ret_ty = machine.adaptor().program.funcs[callee.index()].returns.first().copied();
            compile_call(machine, sym, args, result, ret_ty)?;
            Ok(true)
        }
        Op::VecLoad { .. } | Op::VecStore { .. } | Op::VecBin { .. } | Op::VecShift { .. } | Op::VecCmp { .. } | Op::VecSelect { .. } | Op::VecReduceAdd { .. } | Op::VecMovmask { .. } | Op::VecSplat { .. } | Op::VecExtract { .. } | Op::VecInsert { .. } | Op::VecWiden { .. } | Op::VecNarrow { .. } => compile_vec_op(machine, op, result),
        Op::CallHost { callee, ref args } => {
            let a = machine.adaptor();
            let sym = machine.function_symbol(a.host_func_index(callee.0));
            let ret_ty = a.program.host_fns[callee.index()].ret;
            compile_call(machine, sym, args, result, ret_ty)?;
            Ok(true)
        }
    }
}

/// Same rule as `gravity_interp::canon`: narrow ints sign-extended, Bool 0/1.
fn gravity_interp_canon(ty: Type, bits: u64) -> u64 {
    match ty {
        Type::Bool => bits & 1,
        Type::I8 => bits as u8 as i8 as i64 as u64,
        Type::I16 => bits as u16 as i16 as i64 as u64,
        Type::I32 => bits as u32 as i32 as i64 as u64,
        _ => bits,
    }
}

/// The constant folded into `op`'s immediate, if the view decided so (the
/// constant is then never materialized, so it must be folded).
fn folded_imm<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: &Op) -> Option<i64> {
    folded_imm_operand(machine, op).map(|(k, _)| k)
}

/// `folded_imm` with the constant's value: the frontend takes the other
/// operand as the register source.
fn folded_imm_operand<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: &Op) -> Option<(i64, gravity_ir::Value)> {
    let a = machine.adaptor();
    let view = &a.view;
    let cv = crate::view::imm_operand(a.func(), &view.const_of, op, a.rules).filter(|c| view.const_fused[c.index()])?;
    let (cty, cbits) = view.const_of[cv.index()].unwrap();
    Some((crate::view::const_canon(cty, cbits), cv))
}

fn compile_call<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, sym: SymRef, args: &[gravity_ir::Value], result: Option<GValue>, ret_ty: Option<Type>) -> PgResult<()>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + X64CompilerState<Frontend = GravityFrontendX64>,
{
    let arguments = args.iter().map(|v| CallArg::new(GValue(v.0)));
    match result {
        Some(r) => {
            let mut r_ref = machine.result_ref(r)?;
            calls::generate_call_iter(machine, CallTarget::Symbol(sym), arguments, Some(&mut r_ref), false)?;
            // The callee does not promise canonical upper bits for narrow results.
            if let Some(ty) = ret_ty {
                if matches!(ty, Type::Bool | Type::I8 | Type::I16 | Type::I32) {
                    let mut rp = r_ref.part(0);
                    let reg = rp.load_to_reg()?;
                    canon(machine, reg, ty)?;
                    rp.set_modified();
                }
            }
        }
        None => {
            calls::generate_call_iter(machine, CallTarget::Symbol(sym), arguments, None, false)?;
        }
    }
    Ok(())
}

fn compile_int_bin<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: IntBinOp, ty: Type, cty: Type, a: GValue, b: GValue, result: GValue) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let w = bits(ty);
    match op {
        IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul | IntBinOp::And | IntBinOp::Or | IntBinOp::Xor => binary(machine, a, b, result, |m, rd, rn, rm| {
            two_addr_gp(m, rd, rn, rm, op != IntBinOp::Sub, |m, d, s| {
                let (d, s) = (gp(d), gp(s));
                match op {
                    IntBinOp::Add => insn!(m, fe64_ADD64rr, d, s),
                    IntBinOp::Sub => insn!(m, fe64_SUB64rr, d, s),
                    IntBinOp::Mul => insn!(m, fe64_IMUL64rr, d, s),
                    IntBinOp::And => insn!(m, fe64_AND64rr, d, s),
                    IntBinOp::Or => insn!(m, fe64_OR64rr, d, s),
                    _ => insn!(m, fe64_XOR64rr, d, s),
                }
            })?;
            // Bool operands are 0/1, so and/or/xor results already are.
            if ty == Type::Bool { Ok(()) } else { canon(m, rd, cty) }
        }),
        // canonical (sign-extended) operands order the same way at 64 bits, signed and unsigned
        IntBinOp::Smin | IntBinOp::Smax | IntBinOp::Umin | IntBinOp::Umax => binary(machine, a, b, result, |m, rd, rn, rm| {
            // `rd` may alias `rn` (reuse) but never `rm`
            mov_gp(m, rd, rn)?;
            insn!(m, fe64_CMP64rr, gp(rd), gp(rm))?;
            let take_m = match op {
                IntBinOp::Smin => Jump::jg,
                IntBinOp::Smax => Jump::jl,
                IntBinOp::Umin => Jump::ja,
                _ => Jump::jb,
            };
            cmov(m, take_m, rd, rm)
        }),
        IntBinOp::Sdiv | IntBinOp::Srem | IntBinOp::Udiv | IntBinOp::Urem => {
            let signed = matches!(op, IntBinOp::Sdiv | IntBinOp::Srem);
            let rem = matches!(op, IntBinOp::Srem | IntBinOp::Urem);
            // Claim RAX:RDX before touching the operands so they load elsewhere.
            let mut sax = ScratchReg::new(machine);
            let ax = sax.alloc_specific(regs::AX)?;
            let mut sdx = ScratchReg::new(machine);
            let dx = sdx.alloc_specific(regs::DX)?;
            {
                let a_ref = machine.value_ref(a)?;
                let mut ap = a_ref.part(0);
                let b_ref = machine.value_ref(b)?;
                let mut bp = b_ref.part(0);
                let ra = ap.load_to_reg()?;
                let rb = bp.load_to_reg()?;
                if signed {
                    mov_gp(machine, ax, ra)?;
                    insn!(machine, fe64_CQO)?;
                    insn!(machine, fe64_IDIV64r, gp(rb))?;
                } else {
                    let mut sb = ScratchReg::new(machine);
                    let divisor = if w < 64 {
                        let tb = sb.alloc_gp()?;
                        zext_into(machine, ax, ra, ty)?;
                        zext_into(machine, tb, rb, ty)?;
                        tb
                    } else {
                        mov_gp(machine, ax, ra)?;
                        rb
                    };
                    insn!(machine, fe64_XOR32rr, gp(dx), gp(dx))?;
                    insn!(machine, fe64_DIV64r, gp(divisor))?;
                }
            }
            let r_ref = machine.result_ref(result)?;
            let mut rp = r_ref.part(0);
            let rd = rp.alloc_reg()?;
            mov_gp(machine, rd, if rem { dx } else { ax })?;
            canon(machine, rd, cty)?;
            rp.set_modified();
            Ok(())
        }
        IntBinOp::Smulh | IntBinOp::Umulh => {
            // One-operand `imul` / `mul`: RDX:RAX = RAX × r (I64 only, no canon).
            let mut sax = ScratchReg::new(machine);
            let ax = sax.alloc_specific(regs::AX)?;
            let mut sdx = ScratchReg::new(machine);
            let dx = sdx.alloc_specific(regs::DX)?;
            {
                let a_ref = machine.value_ref(a)?;
                let mut ap = a_ref.part(0);
                let b_ref = machine.value_ref(b)?;
                let mut bp = b_ref.part(0);
                let ra = ap.load_to_reg()?;
                let rb = bp.load_to_reg()?;
                mov_gp(machine, ax, ra)?;
                if op == IntBinOp::Smulh {
                    insn!(machine, fe64_IMUL64r, gp(rb))?;
                } else {
                    insn!(machine, fe64_MUL64r, gp(rb))?;
                }
            }
            let r_ref = machine.result_ref(result)?;
            let mut rp = r_ref.part(0);
            let rd = rp.alloc_reg()?;
            mov_gp(machine, rd, dx)?;
            rp.set_modified();
            Ok(())
        }
        IntBinOp::Rotl => {
            // I64 only (verify); the variable count lives in CL, taken modulo 64 by the instruction.
            let mut scx = ScratchReg::new(machine);
            let cx = scx.alloc_specific(regs::CX)?;
            binary(machine, a, b, result, |m, rd, rn, rm| {
                mov_gp(m, cx, rm)?;
                mov_gp(m, rd, rn)?;
                insn!(m, fe64_ROL64rr, gp(rd), gp(cx))
            })
        }
        IntBinOp::Shl | IntBinOp::Ushr | IntBinOp::Sshr => {
            // Variable shift counts live in CL.
            let mut scx = ScratchReg::new(machine);
            let cx = scx.alloc_specific(regs::CX)?;
            binary(machine, a, b, result, |m, rd, rn, rm| {
                mov_gp(m, cx, rm)?;
                let (d, c) = (gp(rd), gp(cx));
                match w {
                    64 => {
                        mov_gp(m, rd, rn)?;
                        match op {
                            IntBinOp::Shl => insn!(m, fe64_SHL64rr, d, c),
                            IntBinOp::Ushr => insn!(m, fe64_SHR64rr, d, c),
                            _ => insn!(m, fe64_SAR64rr, d, c),
                        }
                    }
                    32 => {
                        mov_gp(m, rd, rn)?;
                        match op {
                            IntBinOp::Shl => insn!(m, fe64_SHL32rr, d, c)?,
                            IntBinOp::Ushr => insn!(m, fe64_SHR32rr, d, c)?,
                            _ => insn!(m, fe64_SAR32rr, d, c)?,
                        }
                        canon(m, rd, cty)
                    }
                    _ => {
                        insn!(m, fe64_AND32ri, c, (w - 1) as i32)?;
                        match op {
                            IntBinOp::Shl => {
                                mov_gp(m, rd, rn)?;
                                insn!(m, fe64_SHL64rr, d, c)?;
                            }
                            IntBinOp::Ushr => {
                                zext_into(m, rd, rn, ty)?;
                                insn!(m, fe64_SHR64rr, d, c)?;
                            }
                            _ => {
                                mov_gp(m, rd, rn)?;
                                insn!(m, fe64_SAR64rr, d, c)?;
                            }
                        }
                        canon(m, rd, cty)
                    }
                }
            })
        }
    }
}

fn compile_int_ovf<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: IntOvfOp, ty: Type, a: GValue, b: GValue, result: GValue) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let w = bits(ty);
    binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
        let mut tmp = ScratchReg::new(m);
        let t = tmp.alloc_gp()?;
        let (tg, n, r) = (gp(t), gp(rn), gp(rm));
        match w {
            64 => {
                insn!(m, fe64_MOV64rr, tg, n)?;
                match op {
                    IntOvfOp::SaddOvf => insn!(m, fe64_ADD64rr, tg, r)?,
                    IntOvfOp::SsubOvf => insn!(m, fe64_SUB64rr, tg, r)?,
                    IntOvfOp::SmulOvf => insn!(m, fe64_IMUL64rr, tg, r)?,
                }
                set_cc(m, Jump::jo, rd)
            }
            32 => {
                insn!(m, fe64_MOV32rr, tg, n)?;
                match op {
                    IntOvfOp::SaddOvf => insn!(m, fe64_ADD32rr, tg, r)?,
                    IntOvfOp::SsubOvf => insn!(m, fe64_SUB32rr, tg, r)?,
                    IntOvfOp::SmulOvf => insn!(m, fe64_IMUL32rr, tg, r)?,
                }
                set_cc(m, Jump::jo, rd)
            }
            _ => {
                // Exact 64-bit result of sign-extended operands, compared with
                // its truncation to the type.
                insn!(m, fe64_MOV64rr, tg, n)?;
                match op {
                    IntOvfOp::SaddOvf => insn!(m, fe64_ADD64rr, tg, r)?,
                    IntOvfOp::SsubOvf => insn!(m, fe64_SUB64rr, tg, r)?,
                    IntOvfOp::SmulOvf => insn!(m, fe64_IMUL64rr, tg, r)?,
                }
                mov_gp(m, rd, t)?;
                canon(m, rd, ty)?;
                insn!(m, fe64_CMP64rr, gp(rd), tg)?;
                set_cc(m, Jump::jne, rd)
            }
        }
    })
}

fn compile_int_un<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: IntUnOp, ty: Type, a: GValue, result: GValue) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let w = bits(ty);
    let popcnt = has_feat(machine, CPU_POPCNT);
    unary(machine, a, result, |m, rd, rn| {
        let d = gp(rd);
        match op {
            IntUnOp::Neg => {
                mov_gp(m, rd, rn)?;
                insn!(m, fe64_NEG64r, d)?;
                canon(m, rd, ty)
            }
            IntUnOp::Not => {
                mov_gp(m, rd, rn)?;
                if ty == Type::Bool {
                    insn!(m, fe64_XOR32ri, d, 1)
                } else {
                    insn!(m, fe64_NOT64r, d)?;
                    canon(m, rd, ty)
                }
            }
            IntUnOp::Clz => {
                // `bsr` leaves the destination undefined on zero input and
                // sets ZF; `cmovne` picks the index only when it is defined.
                let mut tmp = ScratchReg::new(m);
                let t = tmp.alloc_gp()?;
                match w {
                    64 => {
                        insn!(m, fe64_BSR64rr, gp(t), gp(rn))?;
                        insn!(m, fe64_MOV32ri, d, 127)?;
                        cmov(m, Jump::jne, rd, t)?;
                        insn!(m, fe64_XOR32ri, d, 63)
                    }
                    32 => {
                        insn!(m, fe64_BSR32rr, gp(t), gp(rn))?;
                        insn!(m, fe64_MOV32ri, d, 63)?;
                        cmov(m, Jump::jne, rd, t)?;
                        insn!(m, fe64_XOR32ri, d, 31)
                    }
                    _ => {
                        zext_into(m, t, rn, ty)?;
                        insn!(m, fe64_BSR32rr, gp(t), gp(t))?;
                        insn!(m, fe64_MOV32ri, d, 63)?;
                        cmov(m, Jump::jne, rd, t)?;
                        insn!(m, fe64_XOR32ri, d, 31)?;
                        insn!(m, fe64_SUB32ri, d, (32 - w) as i32)
                    }
                }
            }
            IntUnOp::Ctz => {
                // Sign extension adds no low set bits, so `bsf` on the
                // canonical value is exact; zero maps to the width.
                let mut tmp = ScratchReg::new(m);
                let t = tmp.alloc_gp()?;
                insn!(m, fe64_BSF64rr, gp(t), gp(rn))?;
                insn!(m, fe64_MOV32ri, d, w as i32)?;
                cmov(m, Jump::jne, rd, t)
            }
            IntUnOp::Popcnt => {
                zext_into(m, rd, rn, ty)?;
                if popcnt {
                    insn!(m, fe64_POPCNT64rr, d, d)
                } else {
                    // Portable SWAR popcount.
                    let mut t1 = ScratchReg::new(m);
                    let mut t2 = ScratchReg::new(m);
                    let t = t1.alloc_gp()?;
                    let k = t2.alloc_gp()?;
                    let (tg, kg) = (gp(t), gp(k));
                    insn!(m, fe64_MOV64rr, tg, d)?;
                    insn!(m, fe64_SHR64ri, tg, 1)?;
                    mov_imm(m, k, 0x5555_5555_5555_5555)?;
                    insn!(m, fe64_AND64rr, tg, kg)?;
                    insn!(m, fe64_SUB64rr, d, tg)?;
                    mov_imm(m, k, 0x3333_3333_3333_3333)?;
                    insn!(m, fe64_MOV64rr, tg, d)?;
                    insn!(m, fe64_AND64rr, tg, kg)?;
                    insn!(m, fe64_SHR64ri, d, 2)?;
                    insn!(m, fe64_AND64rr, d, kg)?;
                    insn!(m, fe64_ADD64rr, d, tg)?;
                    insn!(m, fe64_MOV64rr, tg, d)?;
                    insn!(m, fe64_SHR64ri, tg, 4)?;
                    insn!(m, fe64_ADD64rr, d, tg)?;
                    mov_imm(m, k, 0x0f0f_0f0f_0f0f_0f0f)?;
                    insn!(m, fe64_AND64rr, d, kg)?;
                    mov_imm(m, k, 0x0101_0101_0101_0101)?;
                    insn!(m, fe64_IMUL64rr, d, kg)?;
                    insn!(m, fe64_SHR64ri, d, 56)
                }
            }
            IntUnOp::Bitrev => {
                // Byte swap, then the nibbles, pairs and bits swapped by
                // mask and shift; the width's shift drops the sign bits.
                let mut t1 = ScratchReg::new(m);
                let mut t2 = ScratchReg::new(m);
                let t = t1.alloc_gp()?;
                let k = t2.alloc_gp()?;
                let (tg, kg) = (gp(t), gp(k));
                mov_gp(m, rd, rn)?;
                insn!(m, fe64_BSWAP64r, d)?;
                for (sh, mask) in [(4i8, 0x0f0f_0f0f_0f0f_0f0fu64), (2, 0x3333_3333_3333_3333), (1, 0x5555_5555_5555_5555)] {
                    mov_imm(m, k, mask)?;
                    insn!(m, fe64_MOV64rr, tg, d)?;
                    insn!(m, fe64_SHR64ri, tg, sh)?;
                    insn!(m, fe64_AND64rr, tg, kg)?;
                    insn!(m, fe64_AND64rr, d, kg)?;
                    insn!(m, fe64_SHL64ri, d, sh)?;
                    insn!(m, fe64_OR64rr, d, tg)?;
                }
                if w < 64 {
                    insn!(m, fe64_SHR64ri, d, (64 - w) as i8)?;
                    canon(m, rd, ty)
                } else {
                    Ok(())
                }
            }
            IntUnOp::Bswap => match w {
                64 => {
                    mov_gp(m, rd, rn)?;
                    insn!(m, fe64_BSWAP64r, d)
                }
                32 => {
                    mov_gp(m, rd, rn)?;
                    insn!(m, fe64_BSWAP32r, d)?;
                    canon(m, rd, ty)
                }
                16 => {
                    insn!(m, fe64_MOV32rr, d, gp(rn))?;
                    insn!(m, fe64_BSWAP32r, d)?;
                    insn!(m, fe64_SAR32ri, d, 16)?;
                    canon(m, rd, ty)
                }
                _ => mov_gp(m, rd, rn),
            },
        }
    })
}

fn compile_float_bin<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: FloatBinOp, ty: Type, a: GValue, b: GValue, result: GValue) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let d = ty == Type::F64;
    let size = ty.bytes();
    match op {
        FloatBinOp::Copysign => {
            // (a & ~sign) | (b & sign) through GP registers.
            binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
                let mut t1 = ScratchReg::new(m);
                let mut t2 = ScratchReg::new(m);
                let x = t1.alloc_gp()?;
                let y = t2.alloc_gp()?;
                fp_to_gp(m, x, rn, size)?;
                fp_to_gp(m, y, rm, size)?;
                if d {
                    let mut t3 = ScratchReg::new(m);
                    let k = t3.alloc_gp()?;
                    mov_imm(m, k, 0x7fff_ffff_ffff_ffff)?;
                    insn!(m, fe64_AND64rr, gp(x), gp(k))?;
                    mov_imm(m, k, 0x8000_0000_0000_0000)?;
                    insn!(m, fe64_AND64rr, gp(y), gp(k))?;
                } else {
                    insn!(m, fe64_AND32ri, gp(x), 0x7fff_ffff)?;
                    insn!(m, fe64_AND32ri, gp(y), i32::MIN)?;
                }
                insn!(m, fe64_OR64rr, gp(x), gp(y))?;
                gp_to_fp(m, rd, x, size)
            })
        }
        FloatBinOp::Min | FloatBinOp::Max => {
            // Rust `f64::min`/`max` (LLVM minnum): return the other operand
            // when one is NaN; on an equal pair (the signed zeros) the x86
            // host returns the second operand, which is exactly what
            // `minsd a, b` does (it also returns `b` when either is NaN), so
            // only a NaN in `b` needs a `cmovp` fix-up. (AArch64 `fminnm`
            // orders the zeros instead; the IR defers to the host's Rust.)
            binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
                let (vd, vm) = (xmm(rd), xmm(rm));
                mov_fp(m, rd, rn)?;
                match (op, d) {
                    (FloatBinOp::Min, true) => insn!(m, fe64_SSE_MINSDrr, vd, vm)?,
                    (FloatBinOp::Min, false) => insn!(m, fe64_SSE_MINSSrr, vd, vm)?,
                    (_, true) => insn!(m, fe64_SSE_MAXSDrr, vd, vm)?,
                    (_, false) => insn!(m, fe64_SSE_MAXSSrr, vd, vm)?,
                }
                if d {
                    insn!(m, fe64_SSE_UCOMISDrr, vm, vm)?;
                } else {
                    insn!(m, fe64_SSE_UCOMISSrr, vm, vm)?;
                }
                let mut t1 = ScratchReg::new(m);
                let mut t2 = ScratchReg::new(m);
                let x = t1.alloc_gp()?;
                let y = t2.alloc_gp()?;
                fp_to_gp(m, x, rd, size)?;
                fp_to_gp(m, y, rn, size)?;
                cmov(m, Jump::jp, x, y)?;
                gp_to_fp(m, rd, x, size)
            })
        }
        _ => binary(machine, a, b, result, |m, rd, rn, rm| {
            let commutative = matches!(op, FloatBinOp::Add | FloatBinOp::Mul);
            two_addr_fp(m, rd, rn, rm, commutative, |m, x, y| {
                let (x, y) = (xmm(x), xmm(y));
                match (op, d) {
                    (FloatBinOp::Add, true) => insn!(m, fe64_SSE_ADDSDrr, x, y),
                    (FloatBinOp::Add, false) => insn!(m, fe64_SSE_ADDSSrr, x, y),
                    (FloatBinOp::Sub, true) => insn!(m, fe64_SSE_SUBSDrr, x, y),
                    (FloatBinOp::Sub, false) => insn!(m, fe64_SSE_SUBSSrr, x, y),
                    (FloatBinOp::Mul, true) => insn!(m, fe64_SSE_MULSDrr, x, y),
                    (FloatBinOp::Mul, false) => insn!(m, fe64_SSE_MULSSrr, x, y),
                    (FloatBinOp::Div, true) => insn!(m, fe64_SSE_DIVSDrr, x, y),
                    (FloatBinOp::Div, false) => insn!(m, fe64_SSE_DIVSSrr, x, y),
                    _ => unreachable!(),
                }
            })
        }),
    }
}

fn compile_float_un<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: FloatUnOp, ty: Type, a: GValue, result: GValue) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let d = ty == Type::F64;
    let size = ty.bytes();
    let sse41 = has_feat(machine, CPU_SSE4_1);
    unary(machine, a, result, |m, rd, rn| {
        let (vd, vn) = (xmm(rd), xmm(rn));
        match op {
            FloatUnOp::Neg | FloatUnOp::Abs => {
                // Sign-bit arithmetic through a GP register.
                let mut tmp = ScratchReg::new(m);
                let t = tmp.alloc_gp()?;
                fp_to_gp(m, t, rn, size)?;
                match (op, d) {
                    (FloatUnOp::Neg, true) => {
                        let mut t2 = ScratchReg::new(m);
                        let k = t2.alloc_gp()?;
                        mov_imm(m, k, 0x8000_0000_0000_0000)?;
                        insn!(m, fe64_XOR64rr, gp(t), gp(k))?;
                    }
                    (FloatUnOp::Neg, false) => insn!(m, fe64_XOR32ri, gp(t), i32::MIN)?,
                    (_, true) => {
                        let mut t2 = ScratchReg::new(m);
                        let k = t2.alloc_gp()?;
                        mov_imm(m, k, 0x7fff_ffff_ffff_ffff)?;
                        insn!(m, fe64_AND64rr, gp(t), gp(k))?;
                    }
                    (_, false) => insn!(m, fe64_AND32ri, gp(t), 0x7fff_ffff)?,
                }
                gp_to_fp(m, rd, t, size)
            }
            FloatUnOp::Sqrt => {
                if d {
                    insn!(m, fe64_SSE_SQRTSDrr, vd, vn)
                } else {
                    insn!(m, fe64_SSE_SQRTSSrr, vd, vn)
                }
            }
            FloatUnOp::Ceil | FloatUnOp::Floor | FloatUnOp::Trunc | FloatUnOp::Nearest => {
                if !sse41 {
                    return Err(invalid("gravity_tpde: float rounding needs SSE4.1 (x86-64-v2)"));
                }
                // Bit 3 suppresses precision exceptions; bits 0-1 pick the mode.
                let imm: i8 = match op {
                    FloatUnOp::Nearest => 0b1000,
                    FloatUnOp::Floor => 0b1001,
                    FloatUnOp::Ceil => 0b1010,
                    _ => 0b1011,
                };
                if d {
                    insn!(m, fe64_SSE_ROUNDSDrri, vd, vn, imm)
                } else {
                    insn!(m, fe64_SSE_ROUNDSSrri, vd, vn, imm)
                }
            }
        }
    })
}

fn compile_conv<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: ConvOp, from: Type, to: Type, a: GValue, result: GValue) -> PgResult<()>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + X64CompilerState<Frontend = GravityFrontendX64>,
{
    let same_bank = from.is_float() == to.is_float();
    let emit_mov = |m: &M, rd: Reg, rn: Reg| -> PgResult<()> {
        if to.is_float() {
            mov_fp(m, rd, rn)
        } else {
            mov_gp(m, rd, rn)
        }
    };
    let go = |f: &dyn Fn(&M, Reg, Reg) -> PgResult<()>| -> PgResult<()> {
        if same_bank {
            unary(machine, a, result, |m, rd, rn| f(m, rd, rn))
        } else {
            unary_cross(machine, a, result, |m, rd, rn| f(m, rd, rn))
        }
    };
    match op {
        ConvOp::Sext | ConvOp::PtrToInt | ConvOp::IntToPtr => go(&emit_mov),
        ConvOp::Zext => go(&|m, rd, rn| zext_into(m, rd, rn, from)),
        ConvOp::Trunc => go(&|m, rd, rn| {
            emit_mov(m, rd, rn)?;
            canon(m, rd, to)
        }),
        ConvOp::SintToFloat => go(&|m, rd, rn| cvt_si2f(m, rd, rn, to)),
        ConvOp::UintToFloat => go(&|m, rd, rn| {
            if from != Type::I64 {
                let mut tmp = ScratchReg::new(m);
                let t = tmp.alloc_gp()?;
                zext_into(m, t, rn, from)?;
                return cvt_si2f(m, rd, t, to);
            }
            // 64-bit unsigned: halve with a sticky bit when bit 63 is set,
            // convert, and double.
            let mut t1 = ScratchReg::new(m);
            let mut t2 = ScratchReg::new(m);
            let t = t1.alloc_gp()?;
            let u = t2.alloc_gp()?;
            let big = m.label_create()?;
            let done = m.label_create()?;
            insn!(m, fe64_TEST64rr, gp(rn), gp(rn))?;
            emission::generate_raw_jump(m, Jump::js, big)?;
            cvt_si2f(m, rd, rn, to)?;
            emission::generate_raw_jump(m, Jump::jmp, done)?;
            m.label_place(big)?;
            insn!(m, fe64_MOV64rr, gp(t), gp(rn))?;
            insn!(m, fe64_SHR64ri, gp(t), 1)?;
            insn!(m, fe64_MOV64rr, gp(u), gp(rn))?;
            insn!(m, fe64_AND32ri, gp(u), 1)?;
            insn!(m, fe64_OR64rr, gp(t), gp(u))?;
            cvt_si2f(m, rd, t, to)?;
            if to == Type::F64 {
                insn!(m, fe64_SSE_ADDSDrr, xmm(rd), xmm(rd))?;
            } else {
                insn!(m, fe64_SSE_ADDSSrr, xmm(rd), xmm(rd))?;
            }
            m.label_place(done)
        }),
        ConvOp::FloatToSint => go(&|m, rd, rn| {
            cvt_f2si(m, rd, rn, from)?;
            canon(m, rd, to)
        }),
        ConvOp::FloatToUint => go(&|m, rd, rn| {
            // Values >= 2^63: subtract 2^63, convert, flip the top bit.
            let size = from.bytes();
            let mut t1 = ScratchReg::new(m);
            let mut f1 = ScratchReg::new(m);
            let mut f2 = ScratchReg::new(m);
            let g = t1.alloc_gp()?;
            let k = f1.alloc(FP_BANK)?;
            let t = f2.alloc(FP_BANK)?;
            let small = m.label_create()?;
            let done = m.label_create()?;
            mov_imm(m, g, if from == Type::F64 { 0x43e0_0000_0000_0000 } else { 0x5f00_0000 })?;
            gp_to_fp(m, k, g, size)?;
            if from == Type::F64 {
                insn!(m, fe64_SSE_UCOMISDrr, xmm(rn), xmm(k))?;
            } else {
                insn!(m, fe64_SSE_UCOMISSrr, xmm(rn), xmm(k))?;
            }
            emission::generate_raw_jump(m, Jump::jb, small)?;
            mov_fp(m, t, rn)?;
            if from == Type::F64 {
                insn!(m, fe64_SSE_SUBSDrr, xmm(t), xmm(k))?;
            } else {
                insn!(m, fe64_SSE_SUBSSrr, xmm(t), xmm(k))?;
            }
            cvt_f2si(m, rd, t, from)?;
            mov_imm(m, g, 0x8000_0000_0000_0000)?;
            insn!(m, fe64_XOR64rr, gp(rd), gp(g))?;
            emission::generate_raw_jump(m, Jump::jmp, done)?;
            m.label_place(small)?;
            cvt_f2si(m, rd, rn, from)?;
            m.label_place(done)?;
            canon(m, rd, to)
        }),
        ConvOp::Fpromote => go(&|m, rd, rn| insn!(m, fe64_SSE_CVTSS2SDrr, xmm(rd), xmm(rn))),
        ConvOp::Fdemote => go(&|m, rd, rn| insn!(m, fe64_SSE_CVTSD2SSrr, xmm(rd), xmm(rn))),
        ConvOp::Bitcast => go(&|m, rd, rn| match (from, to) {
            (Type::F64, _) => fp_to_gp(m, rd, rn, 8),
            (Type::F32, _) => {
                fp_to_gp(m, rd, rn, 4)?;
                canon(m, rd, to)
            }
            (_, Type::F64) => gp_to_fp(m, rd, rn, 8),
            (_, Type::F32) => gp_to_fp(m, rd, rn, 4),
            _ => emit_mov(m, rd, rn),
        }),
    }
}

/// Signed 64-bit integer to float.
fn cvt_si2f<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, rd: Reg, rn: Reg, to: Type) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if to == Type::F64 {
        insn!(m, fe64_SSE_CVTSI2SD64rr, xmm(rd), gp(rn))
    } else {
        insn!(m, fe64_SSE_CVTSI2SS64rr, xmm(rd), gp(rn))
    }
}

/// Float to signed 64-bit integer, truncating.
fn cvt_f2si<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, rd: Reg, rn: Reg, from: Type) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if from == Type::F64 {
        insn!(m, fe64_SSE_CVTTSD2SI64rr, gp(rd), xmm(rn))
    } else {
        insn!(m, fe64_SSE_CVTTSS2SI64rr, gp(rd), xmm(rn))
    }
}

fn compile_term<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, block: u32) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + X64CompilerState<Frontend = GravityFrontendX64>,
{
    let adaptor = machine.adaptor();
    let func = adaptor.func();
    let succs = mcx::to_vec_in(&adaptor.view.blocks[block as usize].succs, adaptor.view.mcx());
    match &func.blocks[block as usize].term {
        Terminator::Br(_) => {
            branches::generate_uncond_branch(machine, GBlock(succs[0]))?;
        }
        Terminator::CondBr { .. } if succs.len() == 1 => {
            branches::generate_uncond_branch(machine, GBlock(succs[0]))?;
        }
        Terminator::CondBr { cond, .. } => {
            let jump = if let Some(f) = adaptor.view.term_fuse[block as usize] {
                let a_ref = machine.value_ref(GValue(f.a.0))?;
                let mut ap = a_ref.part(0);
                let ra = ap.load_to_reg()?;
                match (f.icond, f.b) {
                    (Some(c), None) if f.self_ext.is_some() => {
                        let mut tmp = ScratchReg::new(machine);
                        let t = tmp.alloc_gp()?;
                        match f.self_ext.unwrap() {
                            8 => insn!(machine, fe64_MOVSXr64r8, gp(t), gp(ra))?,
                            16 => insn!(machine, fe64_MOVSXr64r16, gp(t), gp(ra))?,
                            _ => insn!(machine, fe64_MOVSXr64r32, gp(t), gp(ra))?,
                        }
                        insn!(machine, fe64_CMP64rr, gp(ra), gp(t))?;
                        icond(c)
                    }
                    (None, Some(b)) if f.ovf.is_some() => {
                        let (op, w) = f.ovf.unwrap();
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        let mut tmp = ScratchReg::new(machine);
                        let t = tmp.alloc_gp()?;
                        let (tg, n, r) = (gp(t), gp(ra), gp(rb));
                        if w == 64 {
                            insn!(machine, fe64_MOV64rr, tg, n)?;
                            match op {
                                IntOvfOp::SaddOvf => insn!(machine, fe64_ADD64rr, tg, r)?,
                                _ => insn!(machine, fe64_SUB64rr, tg, r)?,
                            }
                        } else {
                            insn!(machine, fe64_MOV32rr, tg, n)?;
                            match op {
                                IntOvfOp::SaddOvf => insn!(machine, fe64_ADD32rr, tg, r)?,
                                _ => insn!(machine, fe64_SUB32rr, tg, r)?,
                            }
                        }
                        Jump::jo
                    }
                    (Some(c @ (IntCond::Eq | IntCond::Ne)), None) if f.imm == 0 => {
                        insn!(machine, fe64_TEST64rr, gp(ra), gp(ra))?;
                        icond(c)
                    }
                    (Some(c), None) => {
                        insn!(machine, fe64_CMP64ri, gp(ra), f.imm as i32)?;
                        icond(c)
                    }
                    (Some(c), Some(b)) => {
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        insn!(machine, fe64_CMP64rr, gp(ra), gp(rb))?;
                        icond(c)
                    }
                    (None, Some(b)) => {
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        let cond = f.fcond.unwrap();
                        let (x, y) = if matches!(cond, FloatCond::Lt | FloatCond::Le) { (rb, ra) } else { (ra, rb) };
                        if f.is_f64 {
                            insn!(machine, fe64_SSE_UCOMISDrr, xmm(x), xmm(y))?;
                        } else {
                            insn!(machine, fe64_SSE_UCOMISSrr, xmm(x), xmm(y))?;
                        }
                        match cond {
                            FloatCond::Lt | FloatCond::Gt => Jump::ja,
                            FloatCond::Le | FloatCond::Ge => Jump::jae,
                            FloatCond::Ordered => Jump::jnp,
                            FloatCond::Unordered => Jump::jp,
                            FloatCond::Eq | FloatCond::Ne => {
                                // ZF and PF together: materialize the bool.
                                let mut t1 = ScratchReg::new(machine);
                                let mut t2 = ScratchReg::new(machine);
                                let t = t1.alloc_gp()?;
                                let u = t2.alloc_gp()?;
                                if cond == FloatCond::Eq {
                                    set_cc(machine, Jump::je, t)?;
                                    set_cc(machine, Jump::jnp, u)?;
                                    insn!(machine, fe64_AND64rr, gp(t), gp(u))?;
                                } else {
                                    set_cc(machine, Jump::jne, t)?;
                                    set_cc(machine, Jump::jp, u)?;
                                    insn!(machine, fe64_OR64rr, gp(t), gp(u))?;
                                }
                                insn!(machine, fe64_TEST64rr, gp(t), gp(t))?;
                                Jump::jne
                            }
                        }
                    }
                    (None, None) => unreachable!(),
                }
            } else {
                let c_ref = machine.value_ref(GValue(cond.0))?;
                let mut cp = c_ref.part(0);
                let reg = gp(cp.load_to_reg()?);
                insn!(machine, fe64_TEST64rr, reg, reg)?;
                Jump::jne
            };
            branches::generate_cond_branch(machine, jump, GBlock(succs[0]), GBlock(succs[1]))?;
        }
        Terminator::Switch { value, cases, .. } => {
            let ty = ty_of(machine, GValue(value.0));
            let mut scratch = ScratchReg::new(machine);
            let sreg = scratch.alloc_gp()?;
            {
                let v_ref = machine.value_ref(GValue(value.0))?;
                let mut vp = v_ref.part(0);
                let reg = vp.load_to_reg()?;
                mov_gp(machine, sreg, reg)?;
            }
            let table: mcx::PgVec<(u64, GBlock)> = mcx::CollectIn::collect_in(cases.iter().enumerate().map(|(i, c)| (c.value, GBlock(succs[i]))), adaptor.view.mcx());
            branches::generate_switch(machine, scratch, bits(ty), GBlock(succs[cases.len()]), &table)?;
        }
        Terminator::Return(values) => {
            // System V returns in RAX:RDX and XMM0:XMM1; TPDE asserts past that.
            let fp = values.iter().filter(|v| ty_of(machine, GValue(v.0)).is_float()).count();
            if values.len() - fp > 2 || fp > 2 {
                return Err(invalid("gravity_tpde: System V returns at most two values per register class"));
            }
            machine.with_cur_cc_assigner(|assigner| {
                let mut builder = RetBuilder::new(machine, assigner);
                for v in values.iter() {
                    builder.add(&GValue(v.0))?;
                }
                builder.ret()
            })?;
        }
        Terminator::Unreachable => {
            insn!(machine, fe64_UD2)?;
            machine.release_regs_after_return();
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Vectors (simd.md §2) on SSE4.2 — x86-64-v2, the baseline every x86-64
// server since Nehalem has; no AVX / AVX-512 dispatch (backend.md §6.10). A
// vector is one XMM register (`Parts { size: 16, bank: FP_BANK }`, spilled as
// `movapd` through the compiler's 16-byte, 16-aligned slot). Two-address
// forms throughout: `movapd rd, rn` then `op rd, rm`; masks are the
// `pand` / `pandn` / `por` triple (no `pblendvb`: its mask is pinned to xmm0);
// the unsigned compares and the 64-bit min / max bias both sides by the sign
// bit and use the signed `pcmpgt`; the constants are seeded from `pcmpeqd x, x`.
// ---------------------------------------------------------------------------

const VEC_FEATS: u32 = CPU_SSE4_1 | CPU_SSE4_2;

/// `pcmpeqd x, x`: every bit set — the seed of every vector constant.
fn all_ones<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, x: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    insn!(m, fe64_SSE_PCMPEQDrr, xmm(x), xmm(x))
}

/// The sign bit of every lane of `lane`'s width, into `x`.
fn sign_mask<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, x: Reg, lane: Type) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    all_ones(m, x)?;
    match lane {
        Type::I64 => insn!(m, fe64_SSE_PSLLQri, xmm(x), 63),
        Type::I32 => insn!(m, fe64_SSE_PSLLDri, xmm(x), 31),
        Type::I16 => insn!(m, fe64_SSE_PSLLWri, xmm(x), 15),
        _ => {
            // 0x0001 words -> 0x01 bytes -> 0x80 bytes
            insn!(m, fe64_SSE_PSRLWri, xmm(x), 15)?;
            insn!(m, fe64_SSE_PACKUSWBrr, xmm(x), xmm(x))?;
            insn!(m, fe64_SSE_PSLLWri, xmm(x), 7)
        }
    }
}

/// `x = !x` through `t`.
fn vec_not<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, x: Reg, t: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    all_ones(m, t)?;
    insn!(m, fe64_SSE_PXORrr, xmm(x), xmm(t))
}

/// `d = d == s` per lane.
fn pcmpeq<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, lane: Type, d: Reg, s: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let (d, s) = (xmm(d), xmm(s));
    match lane {
        Type::I64 => insn!(m, fe64_SSE_PCMPEQQrr, d, s),
        Type::I32 => insn!(m, fe64_SSE_PCMPEQDrr, d, s),
        Type::I16 => insn!(m, fe64_SSE_PCMPEQWrr, d, s),
        Type::I8 => insn!(m, fe64_SSE_PCMPEQBrr, d, s),
        _ => Err(invalid("gravity_tpde: vector compare on a float lane")),
    }
}

/// `d = d >s s` per lane.
fn pcmpgt<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, lane: Type, d: Reg, s: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let (d, s) = (xmm(d), xmm(s));
    match lane {
        Type::I64 => insn!(m, fe64_SSE_PCMPGTQrr, d, s),
        Type::I32 => insn!(m, fe64_SSE_PCMPGTDrr, d, s),
        Type::I16 => insn!(m, fe64_SSE_PCMPGTWrr, d, s),
        Type::I8 => insn!(m, fe64_SSE_PCMPGTBrr, d, s),
        _ => Err(invalid("gravity_tpde: vector compare on a float lane")),
    }
}

/// `d = d OP s` for the ops with one SSE instruction.
fn vec_bin_insn<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, op: VecBinOp, lane: Type, d: Reg, s: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let (d, s) = (xmm(d), xmm(s));
    match (op, lane) {
        (VecBinOp::And, _) => insn!(m, fe64_SSE_PANDrr, d, s),
        (VecBinOp::Or, _) => insn!(m, fe64_SSE_PORrr, d, s),
        (VecBinOp::Xor, _) => insn!(m, fe64_SSE_PXORrr, d, s),
        (VecBinOp::Add, Type::I64) => insn!(m, fe64_SSE_PADDQrr, d, s),
        (VecBinOp::Add, Type::I32) => insn!(m, fe64_SSE_PADDDrr, d, s),
        (VecBinOp::Add, Type::I16) => insn!(m, fe64_SSE_PADDWrr, d, s),
        (VecBinOp::Add, Type::I8) => insn!(m, fe64_SSE_PADDBrr, d, s),
        (VecBinOp::Add, Type::F64) => insn!(m, fe64_SSE_ADDPDrr, d, s),
        (VecBinOp::Add, Type::F32) => insn!(m, fe64_SSE_ADDPSrr, d, s),
        (VecBinOp::Sub, Type::I64) => insn!(m, fe64_SSE_PSUBQrr, d, s),
        (VecBinOp::Sub, Type::I32) => insn!(m, fe64_SSE_PSUBDrr, d, s),
        (VecBinOp::Sub, Type::I16) => insn!(m, fe64_SSE_PSUBWrr, d, s),
        (VecBinOp::Sub, Type::I8) => insn!(m, fe64_SSE_PSUBBrr, d, s),
        (VecBinOp::Sub, Type::F64) => insn!(m, fe64_SSE_SUBPDrr, d, s),
        (VecBinOp::Sub, Type::F32) => insn!(m, fe64_SSE_SUBPSrr, d, s),
        (VecBinOp::Mul, Type::I32) => insn!(m, fe64_SSE_PMULLDrr, d, s),
        (VecBinOp::Mul, Type::I16) => insn!(m, fe64_SSE_PMULLWrr, d, s),
        (VecBinOp::Mul, Type::F64) => insn!(m, fe64_SSE_MULPDrr, d, s),
        (VecBinOp::Mul, Type::F32) => insn!(m, fe64_SSE_MULPSrr, d, s),
        (VecBinOp::Smin, Type::I32) => insn!(m, fe64_SSE_PMINSDrr, d, s),
        (VecBinOp::Smax, Type::I32) => insn!(m, fe64_SSE_PMAXSDrr, d, s),
        (VecBinOp::Umin, Type::I32) => insn!(m, fe64_SSE_PMINUDrr, d, s),
        (VecBinOp::Umax, Type::I32) => insn!(m, fe64_SSE_PMAXUDrr, d, s),
        (VecBinOp::Smin, Type::I16) => insn!(m, fe64_SSE_PMINSWrr, d, s),
        (VecBinOp::Smax, Type::I16) => insn!(m, fe64_SSE_PMAXSWrr, d, s),
        (VecBinOp::Umin, Type::I16) => insn!(m, fe64_SSE_PMINUWrr, d, s),
        (VecBinOp::Umax, Type::I16) => insn!(m, fe64_SSE_PMAXUWrr, d, s),
        (VecBinOp::Smin, Type::I8) => insn!(m, fe64_SSE_PMINSBrr, d, s),
        (VecBinOp::Smax, Type::I8) => insn!(m, fe64_SSE_PMAXSBrr, d, s),
        (VecBinOp::Umin, Type::I8) => insn!(m, fe64_SSE_PMINUBrr, d, s),
        (VecBinOp::Umax, Type::I8) => insn!(m, fe64_SSE_PMAXUBrr, d, s),
        _ => Err(invalid("gravity_tpde: vector op has no SSE form for this lane type")),
    }
}

/// `x <<= k` / `x >>= k` (logical / arithmetic) on 16/32/64-bit lanes.
fn vec_shift_insn<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, op: VecShiftOp, lane: Type, x: Reg, k: i8) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let x = xmm(x);
    match (op, lane) {
        (VecShiftOp::Shl, Type::I64) => insn!(m, fe64_SSE_PSLLQri, x, k),
        (VecShiftOp::Shl, Type::I32) => insn!(m, fe64_SSE_PSLLDri, x, k),
        (VecShiftOp::Shl, Type::I16) => insn!(m, fe64_SSE_PSLLWri, x, k),
        (VecShiftOp::Ushr, Type::I64) => insn!(m, fe64_SSE_PSRLQri, x, k),
        (VecShiftOp::Ushr, Type::I32) => insn!(m, fe64_SSE_PSRLDri, x, k),
        (VecShiftOp::Ushr, Type::I16) => insn!(m, fe64_SSE_PSRLWri, x, k),
        (VecShiftOp::Sshr, Type::I32) => insn!(m, fe64_SSE_PSRADri, x, k),
        (VecShiftOp::Sshr, Type::I16) => insn!(m, fe64_SSE_PSRAWri, x, k),
        _ => Err(invalid("gravity_tpde: vector shift on a float lane")),
    }
}

/// `rd = mask ? p : q` with `mask` clobbered (`rd` aliases none of them).
fn vec_blend<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, rd: Reg, mask: Reg, p: Reg, q: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    mov_fp(m, rd, p)?;
    insn!(m, fe64_SSE_PANDrr, xmm(rd), xmm(mask))?;
    insn!(m, fe64_SSE_PANDNrr, xmm(mask), xmm(q))?;
    insn!(m, fe64_SSE_PORrr, xmm(rd), xmm(mask))
}

fn pshufd<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, d: Reg, s: Reg, imm: u8) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    insn!(m, fe64_SSE_PSHUFDrri, xmm(d), xmm(s), imm as i8)
}

/// `d = sum of the four dwords of d` (in dword 0) through `t`.
fn reduce_dwords<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, d: Reg, t: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    pshufd(m, t, d, 0x4E)?;
    insn!(m, fe64_SSE_PADDDrr, xmm(d), xmm(t))?;
    pshufd(m, t, d, 0xB1)?;
    insn!(m, fe64_SSE_PADDDrr, xmm(d), xmm(t))
}

/// The 0x0001 words into `x` (`pmaddwd`'s pair-summing multiplier).
fn ones_words<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(m: &M, x: Reg) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    all_ones(m, x)?;
    insn!(m, fe64_SSE_PSRLWri, xmm(x), 15)
}

fn compile_vec_op<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: &Op, result: Option<GValue>) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + X64CompilerState<Frontend = GravityFrontendX64>,
{
    let g = |v: gravity_ir::Value| GValue(v.0);
    if !has_feat(machine, VEC_FEATS) {
        return Err(invalid("gravity_tpde: vector ops need SSE4.2 (x86-64-v2)"));
    }
    match *op {
        Op::VecLoad { ptr, offset, .. } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let base = pp.load_to_reg()?;
            let dst = rp.alloc_reg()?;
            let a = mem(base, offset)?;
            insn!(machine, fe64_SSE_MOVDQUrm, xmm(dst), a)?;
            rp.set_modified();
            Ok(true)
        }
        Op::VecStore { ptr, offset, value, .. } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let v_ref = machine.value_ref(g(value))?;
            let mut vp = v_ref.part(0);
            let base = pp.load_to_reg()?;
            let src = vp.load_to_reg()?;
            let a = mem(base, offset)?;
            insn!(machine, fe64_SSE_MOVDQUmr, a, xmm(src))?;
            Ok(true)
        }
        Op::VecBin { op: bop @ (VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax), a, b } if ty_of(machine, g(a)).lane() == Type::I64 => {
            // no 64-bit min / max below AVX-512: `pcmpgtq` (biased for the
            // unsigned forms) into a mask, then the blend
            binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                let signed = matches!(bop, VecBinOp::Smin | VecBinOp::Smax);
                let is_max = matches!(bop, VecBinOp::Smax | VecBinOp::Umax);
                if signed {
                    mov_fp(m, t, rn)?;
                    pcmpgt(m, Type::I64, t, rm)?;
                } else {
                    let mut s2 = ScratchReg::new(m);
                    let u = s2.alloc(FP_BANK)?;
                    sign_mask(m, u, Type::I64)?;
                    mov_fp(m, t, rn)?;
                    insn!(m, fe64_SSE_PXORrr, xmm(t), xmm(u))?;
                    mov_fp(m, rd, rm)?;
                    insn!(m, fe64_SSE_PXORrr, xmm(rd), xmm(u))?;
                    pcmpgt(m, Type::I64, t, rd)?;
                }
                // t = a > b: the max takes a there, the min takes b
                let (p, q) = if is_max { (rn, rm) } else { (rm, rn) };
                vec_blend(m, rd, t, p, q)
            })?;
            Ok(true)
        }
        Op::VecBin { op: VecBinOp::Mul, a, b } if ty_of(machine, g(a)).lane() == Type::I8 => {
            // no byte multiply: the odd bytes through `psrlw 8` + `pmullw` +
            // `psllw 8`, the even ones as the low byte of `pmullw`
            binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                let mut s2 = ScratchReg::new(m);
                let u = s2.alloc(FP_BANK)?;
                mov_fp(m, t, rn)?;
                insn!(m, fe64_SSE_PSRLWri, xmm(t), 8)?;
                mov_fp(m, u, rm)?;
                insn!(m, fe64_SSE_PSRLWri, xmm(u), 8)?;
                insn!(m, fe64_SSE_PMULLWrr, xmm(t), xmm(u))?;
                insn!(m, fe64_SSE_PSLLWri, xmm(t), 8)?;
                mov_fp(m, rd, rn)?;
                insn!(m, fe64_SSE_PMULLWrr, xmm(rd), xmm(rm))?;
                all_ones(m, u)?;
                insn!(m, fe64_SSE_PSRLWri, xmm(u), 8)?;
                insn!(m, fe64_SSE_PANDrr, xmm(rd), xmm(u))?;
                insn!(m, fe64_SSE_PORrr, xmm(rd), xmm(t))
            })?;
            Ok(true)
        }
        Op::VecBin { op: bop, a, b } => {
            let lane = ty_of(machine, g(a)).lane();
            let commutative = bop != VecBinOp::Sub;
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| two_addr_fp(m, rd, rn, rm, commutative, |m, d, s| vec_bin_insn(m, bop, lane, d, s)))?;
            Ok(true)
        }
        Op::VecShift { op: sop, a, count } => {
            let lane = ty_of(machine, g(a)).lane();
            let k = count as i8;
            unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                if count == 0 {
                    return mov_fp(m, rd, rn);
                }
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                match (sop, lane) {
                    (VecShiftOp::Sshr, Type::I64) => {
                        // no `psraq`: the logical shift, or'ed with the sign
                        // (broadcast from the high dword) shifted in from the top
                        mov_fp(m, t, rn)?;
                        insn!(m, fe64_SSE_PSRADri, xmm(t), 31)?;
                        pshufd(m, t, t, 0xF5)?;
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PSRLQri, xmm(rd), k)?;
                        insn!(m, fe64_SSE_PSLLQri, xmm(t), 64 - k)?;
                        insn!(m, fe64_SSE_PORrr, xmm(rd), xmm(t))
                    }
                    (VecShiftOp::Shl, Type::I8) => {
                        // the word shift, the bits crossing a byte masked off
                        // by (0xff << k) & 0xff in every byte
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PSLLWri, xmm(rd), k)?;
                        all_ones(m, t)?;
                        insn!(m, fe64_SSE_PSLLWri, xmm(t), 8 + k)?;
                        insn!(m, fe64_SSE_PSRLWri, xmm(t), 8)?;
                        insn!(m, fe64_SSE_PACKUSWBrr, xmm(t), xmm(t))?;
                        insn!(m, fe64_SSE_PANDrr, xmm(rd), xmm(t))
                    }
                    (VecShiftOp::Ushr, Type::I8) => {
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PSRLWri, xmm(rd), k)?;
                        all_ones(m, t)?;
                        insn!(m, fe64_SSE_PSRLWri, xmm(t), 8 + k)?;
                        insn!(m, fe64_SSE_PACKUSWBrr, xmm(t), xmm(t))?;
                        insn!(m, fe64_SSE_PANDrr, xmm(rd), xmm(t))
                    }
                    (VecShiftOp::Sshr, Type::I8) => {
                        // each byte doubled into a word (`punpck?bw x, x`), the
                        // word shifted by 8 + k, the halves packed with saturation
                        // (every result fits a byte)
                        mov_fp(m, t, rn)?;
                        insn!(m, fe64_SSE_PUNPCKHBWrr, xmm(t), xmm(t))?;
                        insn!(m, fe64_SSE_PSRAWri, xmm(t), 8 + k)?;
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PUNPCKLBWrr, xmm(rd), xmm(rd))?;
                        insn!(m, fe64_SSE_PSRAWri, xmm(rd), 8 + k)?;
                        insn!(m, fe64_SSE_PACKSSWBrr, xmm(rd), xmm(t))
                    }
                    _ => {
                        mov_fp(m, rd, rn)?;
                        vec_shift_insn(m, sop, lane, rd, k)
                    }
                }
            })?;
            Ok(true)
        }
        Op::VecCmp { cond, a, b } => {
            let lane = ty_of(machine, g(a)).lane();
            // `lt` / `le` by swapping the operands; `ge` = not `lt`, `ne` = not `eq`
            let (a, b, cond) = match cond {
                IntCond::Slt => (b, a, IntCond::Sgt),
                IntCond::Sle => (b, a, IntCond::Sge),
                IntCond::Ult => (b, a, IntCond::Ugt),
                IntCond::Ule => (b, a, IntCond::Uge),
                c => (a, b, c),
            };
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                match cond {
                    IntCond::Eq | IntCond::Ne => {
                        mov_fp(m, rd, rn)?;
                        pcmpeq(m, lane, rd, rm)?;
                        if cond == IntCond::Ne {
                            vec_not(m, rd, t)?;
                        }
                        Ok(())
                    }
                    IntCond::Sgt => {
                        mov_fp(m, rd, rn)?;
                        pcmpgt(m, lane, rd, rm)
                    }
                    IntCond::Sge => {
                        mov_fp(m, t, rm)?;
                        pcmpgt(m, lane, t, rn)?;
                        all_ones(m, rd)?;
                        insn!(m, fe64_SSE_PXORrr, xmm(rd), xmm(t))
                    }
                    IntCond::Ugt => {
                        let mut s2 = ScratchReg::new(m);
                        let u = s2.alloc(FP_BANK)?;
                        sign_mask(m, t, lane)?;
                        mov_fp(m, u, rm)?;
                        insn!(m, fe64_SSE_PXORrr, xmm(u), xmm(t))?;
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PXORrr, xmm(rd), xmm(t))?;
                        pcmpgt(m, lane, rd, u)
                    }
                    _ => {
                        // Uge: not (b >u a)
                        let mut s2 = ScratchReg::new(m);
                        let u = s2.alloc(FP_BANK)?;
                        sign_mask(m, t, lane)?;
                        mov_fp(m, u, rm)?;
                        insn!(m, fe64_SSE_PXORrr, xmm(u), xmm(t))?;
                        insn!(m, fe64_SSE_PXORrr, xmm(t), xmm(rn))?;
                        pcmpgt(m, lane, u, t)?;
                        all_ones(m, rd)?;
                        insn!(m, fe64_SSE_PXORrr, xmm(rd), xmm(u))
                    }
                }
            })?;
            Ok(true)
        }
        Op::VecSelect { mask, a, b } => {
            let m_ref = machine.value_ref(g(mask))?;
            let mut mp = m_ref.part(0);
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let b_ref = machine.value_ref(g(b))?;
            let mut bp = b_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let mr = mp.load_to_reg()?;
            let ar = ap.load_to_reg()?;
            let br = bp.load_to_reg()?;
            let rd = rp.alloc_reg()?;
            let mut s1 = ScratchReg::new(machine);
            let t = s1.alloc(FP_BANK)?;
            mov_fp(machine, t, mr)?;
            vec_blend(machine, rd, t, ar, br)?;
            rp.set_modified();
            Ok(true)
        }
        Op::VecReduceAdd { a } => {
            let lane = ty_of(machine, g(a)).lane();
            if lane.is_float() {
                unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                    let mut s1 = ScratchReg::new(m);
                    let t = s1.alloc(FP_BANK)?;
                    if lane == Type::F64 {
                        pshufd(m, t, rn, 0xEE)?;
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_ADDSDrr, xmm(rd), xmm(t))
                    } else {
                        // (l0 + l1) + (l2 + l3), the interpreter's pairing
                        pshufd(m, t, rn, 0xB1)?;
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_ADDPSrr, xmm(rd), xmm(t))?;
                        pshufd(m, t, rd, 0x02)?;
                        insn!(m, fe64_SSE_ADDSSrr, xmm(rd), xmm(t))
                    }
                })?;
                return Ok(true);
            }
            unary_cross(machine, g(a), result.unwrap(), |m, rd, rn| {
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                let mut s2 = ScratchReg::new(m);
                let u = s2.alloc(FP_BANK)?;
                match lane {
                    Type::I64 => {
                        pshufd(m, t, rn, 0x4E)?;
                        insn!(m, fe64_SSE_PADDQrr, xmm(t), xmm(rn))?;
                        fp_to_gp(m, rd, t, 8)
                    }
                    Type::I32 => {
                        // the lanes widened to i64 first (the sum is exact)
                        insn!(m, fe64_SSE_PMOVSXDQrr, xmm(t), xmm(rn))?;
                        pshufd(m, u, rn, 0xEE)?;
                        insn!(m, fe64_SSE_PMOVSXDQrr, xmm(u), xmm(u))?;
                        insn!(m, fe64_SSE_PADDQrr, xmm(t), xmm(u))?;
                        pshufd(m, u, t, 0x4E)?;
                        insn!(m, fe64_SSE_PADDQrr, xmm(t), xmm(u))?;
                        fp_to_gp(m, rd, t, 8)
                    }
                    Type::I16 => {
                        // pairs summed into dwords by `pmaddwd` with ones
                        ones_words(m, u)?;
                        mov_fp(m, t, rn)?;
                        insn!(m, fe64_SSE_PMADDWDrr, xmm(t), xmm(u))?;
                        reduce_dwords(m, t, u)?;
                        fp_to_gp(m, rd, t, 4)?;
                        canon(m, rd, Type::I32)
                    }
                    _ => {
                        insn!(m, fe64_SSE_PMOVSXBWrr, xmm(t), xmm(rn))?;
                        pshufd(m, u, rn, 0xEE)?;
                        insn!(m, fe64_SSE_PMOVSXBWrr, xmm(u), xmm(u))?;
                        insn!(m, fe64_SSE_PADDWrr, xmm(t), xmm(u))?;
                        ones_words(m, u)?;
                        insn!(m, fe64_SSE_PMADDWDrr, xmm(t), xmm(u))?;
                        reduce_dwords(m, t, u)?;
                        fp_to_gp(m, rd, t, 4)?;
                        canon(m, rd, Type::I32)
                    }
                }
            })?;
            Ok(true)
        }
        Op::VecMovmask { a } => {
            let lane = ty_of(machine, g(a)).lane();
            unary_cross(machine, g(a), result.unwrap(), |m, rd, rn| match lane {
                Type::I8 => insn!(m, fe64_SSE_PMOVMSKBrr, gp(rd), xmm(rn)),
                Type::I16 => {
                    // words saturated to bytes keep their sign: the byte mask's low 8 bits
                    let mut s1 = ScratchReg::new(m);
                    let t = s1.alloc(FP_BANK)?;
                    mov_fp(m, t, rn)?;
                    insn!(m, fe64_SSE_PACKSSWBrr, xmm(t), xmm(t))?;
                    insn!(m, fe64_SSE_PMOVMSKBrr, gp(rd), xmm(t))?;
                    insn!(m, fe64_AND64ri, gp(rd), 0xff)
                }
                Type::I32 | Type::F32 => insn!(m, fe64_SSE_MOVMSKPSrr, gp(rd), xmm(rn)),
                _ => insn!(m, fe64_SSE_MOVMSKPDrr, gp(rd), xmm(rn)),
            })?;
            Ok(true)
        }
        Op::VecSplat { ty, a } => {
            let lane = ty.lane();
            if lane.is_float() {
                unary(machine, g(a), result.unwrap(), |m, rd, rn| pshufd(m, rd, rn, if lane == Type::F64 { 0x44 } else { 0x00 }))?;
            } else {
                unary_cross(machine, g(a), result.unwrap(), |m, rd, rn| {
                    match lane {
                        Type::I64 => {
                            gp_to_fp(m, rd, rn, 8)?;
                            return pshufd(m, rd, rd, 0x44);
                        }
                        Type::I32 => gp_to_fp(m, rd, rn, 4)?,
                        Type::I16 => {
                            gp_to_fp(m, rd, rn, 4)?;
                            insn!(m, fe64_SSE_PUNPCKLWDrr, xmm(rd), xmm(rd))?;
                        }
                        Type::I8 => {
                            gp_to_fp(m, rd, rn, 4)?;
                            insn!(m, fe64_SSE_PUNPCKLBWrr, xmm(rd), xmm(rd))?;
                            insn!(m, fe64_SSE_PUNPCKLWDrr, xmm(rd), xmm(rd))?;
                        }
                        _ => return Err(invalid("gravity_tpde: vsplat lane type")),
                    }
                    pshufd(m, rd, rd, 0x00)
                })?;
            }
            Ok(true)
        }
        Op::VecExtract { a, lane } => {
            let lt = ty_of(machine, g(a)).lane();
            let i = lane as i8;
            if lt.is_float() {
                unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                    if i == 0 {
                        mov_fp(m, rd, rn)
                    } else {
                        pshufd(m, rd, rn, if lt == Type::F64 { 0xEE } else { i as u8 })
                    }
                })?;
            } else {
                unary_cross(machine, g(a), result.unwrap(), |m, rd, rn| match lt {
                    Type::I64 => insn!(m, fe64_SSE_PEXTRQrri, gp(rd), xmm(rn), i),
                    Type::I32 => {
                        insn!(m, fe64_SSE_PEXTRDrri, gp(rd), xmm(rn), i)?;
                        canon(m, rd, Type::I32)
                    }
                    Type::I16 => {
                        insn!(m, fe64_SSE_PEXTRWrri, gp(rd), xmm(rn), i)?;
                        canon(m, rd, Type::I16)
                    }
                    _ => {
                        insn!(m, fe64_SSE_PEXTRBrri, gp(rd), xmm(rn), i)?;
                        canon(m, rd, Type::I8)
                    }
                })?;
            }
            Ok(true)
        }
        Op::VecInsert { a, lane, value } => {
            let lt = ty_of(machine, g(a)).lane();
            let i = lane as i8;
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let x_ref = machine.value_ref(g(value))?;
            let mut xp = x_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let an = ap.load_to_reg()?;
            let xr = xp.load_to_reg()?;
            let rd = rp.alloc_try_reuse(&mut ap)?;
            mov_fp(machine, rd, an)?;
            match lt {
                Type::I64 => insn!(machine, fe64_SSE_PINSRQrri, xmm(rd), gp(xr), i)?,
                Type::I32 => insn!(machine, fe64_SSE_PINSRDrri, xmm(rd), gp(xr), i)?,
                Type::I16 => insn!(machine, fe64_SSE_PINSRWrri, xmm(rd), gp(xr), i)?,
                Type::I8 => insn!(machine, fe64_SSE_PINSRBrri, xmm(rd), gp(xr), i)?,
                _ => return Err(invalid("gravity_tpde: vinsert of a float lane is not encoded")),
            }
            rp.set_modified();
            Ok(true)
        }
        Op::VecWiden { a, signed, high } => {
            let lt = ty_of(machine, g(a)).lane();
            unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                let src = if high {
                    pshufd(m, rd, rn, 0xEE)?;
                    rd
                } else {
                    rn
                };
                let (d, s) = (xmm(rd), xmm(src));
                match (lt, signed) {
                    (Type::I8, true) => insn!(m, fe64_SSE_PMOVSXBWrr, d, s),
                    (Type::I8, false) => insn!(m, fe64_SSE_PMOVZXBWrr, d, s),
                    (Type::I16, true) => insn!(m, fe64_SSE_PMOVSXWDrr, d, s),
                    (Type::I16, false) => insn!(m, fe64_SSE_PMOVZXWDrr, d, s),
                    (Type::I32, true) => insn!(m, fe64_SSE_PMOVSXDQrr, d, s),
                    (Type::I32, false) => insn!(m, fe64_SSE_PMOVZXDQrr, d, s),
                    _ => Err(invalid("gravity_tpde: vwiden lane type")),
                }
            })?;
            Ok(true)
        }
        Op::VecNarrow { a, b } => {
            let lt = ty_of(machine, g(a)).lane();
            // a truncating narrow (`xtn`): the packs saturate, so the lanes
            // are sign-extended from their low half first (then the signed
            // pack is the identity); the 64-bit lanes by two dword shuffles
            binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let mut s1 = ScratchReg::new(m);
                let t = s1.alloc(FP_BANK)?;
                match lt {
                    Type::I64 => {
                        pshufd(m, rd, rn, 0x08)?;
                        pshufd(m, t, rm, 0x08)?;
                        insn!(m, fe64_SSE_PUNPCKLQDQrr, xmm(rd), xmm(t))
                    }
                    Type::I32 => {
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PSLLDri, xmm(rd), 16)?;
                        insn!(m, fe64_SSE_PSRADri, xmm(rd), 16)?;
                        mov_fp(m, t, rm)?;
                        insn!(m, fe64_SSE_PSLLDri, xmm(t), 16)?;
                        insn!(m, fe64_SSE_PSRADri, xmm(t), 16)?;
                        insn!(m, fe64_SSE_PACKSSDWrr, xmm(rd), xmm(t))
                    }
                    Type::I16 => {
                        mov_fp(m, rd, rn)?;
                        insn!(m, fe64_SSE_PSLLWri, xmm(rd), 8)?;
                        insn!(m, fe64_SSE_PSRAWri, xmm(rd), 8)?;
                        mov_fp(m, t, rm)?;
                        insn!(m, fe64_SSE_PSLLWri, xmm(t), 8)?;
                        insn!(m, fe64_SSE_PSRAWri, xmm(t), 8)?;
                        insn!(m, fe64_SSE_PACKSSWBrr, xmm(rd), xmm(t))
                    }
                    _ => Err(invalid("gravity_tpde: vnarrow lane type")),
                }
            })?;
            Ok(true)
        }
        _ => unreachable!("not a vector op"),
    }
}
