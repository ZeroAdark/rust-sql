//! `A64Frontend` for Gravity IR: one `compile_inst` per Gravity op.
//!
//! Register convention (matches `gravity_interp::canon`): every integer or
//! pointer value lives in a 64-bit GP register, narrow integers sign-extended,
//! `Bool` as 0/1. Floats live in FP registers (`s` for F32, `d` for F64).
//! Callers of compiled functions pass arguments in that form and receive
//! results in that form.

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
use tpde_compiler_a64::{calls, raw, A64CompilerState, A64Frontend, Jump, JumpKind, FP_BANK, GP_BANK};
use tpde_core::{
    compiler::{ValRefSpecial, ValueParts},
    RegBank, Reg,
};
use tpde_disarm::*;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

type A<'p> = GravityAdaptor<'p>;

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
        // A 128-bit vector is one `q` register (16-byte spill slot).
        t if t.is_vector() => Parts { size: 16, bank: FP_BANK },
        _ => Parts { size: 8, bank: GP_BANK },
    }
}

#[derive(Debug, Default)]
pub struct GravityFrontend {
    /// Register-allocation events the compiler reported
    /// (`CompilerTarget::regalloc_event`), per function and original
    /// block: `Compiled::regalloc`. Recorded only under `record` (a
    /// `Scratch::recording` compile, the tests'): production keeps none.
    pub events: core::cell::RefCell<Vec<crate::RegEvent>>,
    pub record: bool,
    /// Nanoseconds in TPDE's analyzer (`analysis_start` .. `analysis_end`)
    /// over every function (`crate::CompileTiming::analysis_ns`).
    pub analysis_ns: core::cell::Cell<u64>,
    analysis_t0: core::cell::Cell<u64>,
}

impl GravityFrontend {
    /// A frontend that records the register-allocation events when `record` (`Scratch::recording`).
    pub fn with_record(record: bool) -> Self {
        GravityFrontend { record, ..Default::default() }
    }
}

/// AArch64 immediate rules: imm12 add/sub/cmp (`cmn` for a negative compare
/// immediate producing a value), logical immediates, `xzr` for `Select`.
pub struct A64Imm;
pub static A64_IMM: A64Imm = A64Imm;
impl crate::view::ImmRules for A64Imm {
    fn add_imm(&self, k: i64) -> bool {
        (-4095..4096).contains(&k)
    }
    fn logical_imm(&self, k: i64) -> bool {
        de64_ANDxi(DA_SP, DA_ZR, k as u64) != 0
    }
    fn cmp_imm(&self, k: i64) -> bool {
        (-4095..4096).contains(&k)
    }
    fn branch_cmp_imm(&self, k: i64) -> bool {
        (0..4096).contains(&k)
    }
    fn select_zero(&self) -> bool {
        true
    }
    fn sext_copy(&self) -> bool {
        true
    }
    fn self_ext_cmp(&self) -> bool {
        true
    }
    fn shifted_operands(&self) -> bool {
        true
    }
    fn address_folds(&self) -> bool {
        true
    }
    fn ccmp(&self) -> bool {
        true
    }
    fn ccmp_imm(&self, k: i64) -> bool {
        (0..32).contains(&k)
    }
    fn store_pairs(&self) -> bool {
        true
    }
    fn phi_const(&self) -> bool {
        true
    }
    fn flag_reuse(&self) -> bool {
        true
    }
    fn bitfield_extract(&self) -> bool {
        true
    }
}

/// The NZCV immediate of a `ccmp` under which `kind` holds (the flags the
/// conditional compare sets when its own condition fails). N = 8, Z = 4,
/// C = 2, V = 1.
fn nzcv_true(kind: JumpKind) -> u32 {
    match kind {
        JumpKind::Eq => 4,
        JumpKind::Ne => 0,
        JumpKind::Cs => 2,
        JumpKind::Cc => 0,
        JumpKind::Mi => 8,
        JumpKind::Pl => 0,
        JumpKind::Vs => 1,
        JumpKind::Vc => 0,
        JumpKind::Hi => 2,
        JumpKind::Ls => 0,
        JumpKind::Ge => 0,
        JumpKind::Lt => 8,
        JumpKind::Gt => 0,
        JumpKind::Le => 4,
        JumpKind::Always | JumpKind::Cbz | JumpKind::Cbnz | JumpKind::Tbz | JumpKind::Tbnz => unreachable!("not a flags condition"),
    }
}

impl<'p> A64Frontend<A<'p>> for GravityFrontend {
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
    const WEIGHTED_REGALLOC: bool = true;
    const PINNED_ASSIGNMENTS: [u32; 2] = [9, 6];
    /// Across a hot call a value may hold a callee-saved register (ten),
    /// a caller-saved non-argument one (x9–x15, x18, x30: nine, saved
    /// around the call) or an argument register but x0/x1 (seven, parked
    /// around the call): 26 of the 28 allocatable. Fourteen classic fixed
    /// (nine nominated invariants, the loop's own values that live past
    /// their block) and nine pinned phis leave five scratch registers.
    const FIXED_ASSIGNMENTS: [u32; 2] = [14, 6];
    const PHI_REUSE: bool = true;
    fn value_weight(&self, adaptor: &A<'p>, value: GValue) -> Option<u32> {
        adaptor.view.weight.get(value.0 as usize).copied()
    }
    fn regalloc_event(&self, adaptor: &A<'p>, event: RegallocEvent, block: GBlock) {
        if self.record {
            self.events.borrow_mut().push(crate::RegEvent::new(adaptor, event, block));
        }
    }
    fn allow_fixed_assignment(&self, adaptor: &A<'p>, value: GValue) -> bool {
        // Fixed assignments pin values to argument registers; a multi-value
        // return then evicts a fixed register (TPDE asserts). Pipelines return
        // one value, so only multi-return functions opt out. A value the
        // view vetoed (`FnView::no_fixed`: never read in a loop) keeps no
        // register through the loops.
        adaptor.func().returns.len() <= 1 && !adaptor.view.no_fixed.get(value.0 as usize).copied().unwrap_or(false)
    }
    fn special_part<'s, 'm, M: TargetMachine<'m, A<'p>, 2>>(_: &'s M, _: &'s Self::Special, _: u32) -> ValuePartRef<'s, M, 2>
    where
        A<'p>: 's,
        M::Target: CompilerTarget<A<'p>, 2, Special = Self::Special> + A64CompilerState,
    {
        unreachable!("gravity has no special values")
    }
    fn compile_inst<'m, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, inst: GInst, _: impl Iterator<Item = GInst> + Clone) -> PgResult<bool>
    where
        M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + A64CompilerState<Frontend = Self>,
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

/// The operand `imm_operand` folds, its reference consumed when the
/// constant is materialized for other uses (the fold never reads the
/// register, and the value must still die where the analyzer says).
fn fold_operand<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: &Op) -> PgResult<Option<gravity_ir::Value>> {
    let view = &machine.adaptor().view;
    let Some(cv) = crate::view::imm_operand(machine.adaptor().func(), &view.const_of, op, machine.adaptor().rules) else {
        return Ok(None);
    };
    if !view.const_fused[cv.index()] {
        drop(machine.value_ref(GValue(cv.0))?);
    }
    Ok(Some(cv))
}

fn ty_of<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, v: GValue) -> Type {
    let a = machine.adaptor();
    a.view.value_type(a.func(), v.0)
}

fn bits(ty: Type) -> u32 {
    ty.bytes() * 8
}

/// Sign-extend `reg` in place to the canonical form of `ty`.
fn canon<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, reg: Reg, ty: Type) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let r = raw::zr(reg)?;
    match ty {
        Type::I8 => raw::emit(machine, de64_SXTBx(r, r)),
        Type::I16 => raw::emit(machine, de64_SXTHx(r, r)),
        Type::I32 => raw::emit(machine, de64_SXTWx(r, r)),
        Type::Bool => raw::emit(machine, de64_UXTBw(r, r)),
        _ => Ok(()),
    }
}

/// Zero-extend `src` (canonical `ty`) into `dst`.
fn zext_into<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg, ty: Type) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let (d, s) = (raw::zr(dst)?, raw::zr(src)?);
    match ty {
        Type::I8 => raw::emit(machine, de64_UXTBw(d, s)),
        Type::I16 => raw::emit(machine, de64_UXTHw(d, s)),
        Type::I32 => raw::emit(machine, de64_MOVw(d, s)),
        _ => {
            if dst != src {
                raw::emit(machine, de64_MOVx(d, s))
            } else {
                Ok(())
            }
        }
    }
}

fn icond(c: IntCond) -> JumpKind {
    match c {
        IntCond::Eq => JumpKind::Eq,
        IntCond::Ne => JumpKind::Ne,
        IntCond::Slt => JumpKind::Lt,
        IntCond::Sle => JumpKind::Le,
        IntCond::Sgt => JumpKind::Gt,
        IntCond::Sge => JumpKind::Ge,
        IntCond::Ult => JumpKind::Cc,
        IntCond::Ule => JumpKind::Ls,
        IntCond::Ugt => JumpKind::Hi,
        IntCond::Uge => JumpKind::Cs,
    }
}

fn fcond(c: FloatCond) -> JumpKind {
    match c {
        FloatCond::Eq => JumpKind::Eq,
        FloatCond::Ne => JumpKind::Ne,
        FloatCond::Lt => JumpKind::Mi,
        FloatCond::Le => JumpKind::Ls,
        FloatCond::Gt => JumpKind::Gt,
        FloatCond::Ge => JumpKind::Ge,
        FloatCond::Ordered => JumpKind::Vc,
        FloatCond::Unordered => JumpKind::Vs,
    }
}

/// NEON `smax`/`umax`/`smin`/`umin` (three-register same, `0 Q U 01110 size 1 Rm 0110 o1 1 Rn Rd`):
/// the pinned Disarm description carries no lane-wise min/max form, so the word is built here.
fn neon_minmax(op: VecBinOp, lane: Type, rd: Reg, rn: Reg, rm: Reg) -> PgResult<u32> {
    for r in [rd, rn, rm] {
        raw::vector(r)?;
    }
    let size: u32 = match lane {
        Type::I8 => 0,
        Type::I16 => 1,
        _ => 2,
    };
    let unsigned = matches!(op, VecBinOp::Umin | VecBinOp::Umax) as u32;
    let min = matches!(op, VecBinOp::Smin | VecBinOp::Umin) as u32;
    let (rd, rn, rm) = ((rd.id() - 32) as u32, (rn.id() - 32) as u32, (rm.id() - 32) as u32);
    Ok(0x4E20_6400 | unsigned << 29 | size << 22 | rm << 16 | min << 11 | rn << 5 | rd)
}

fn cond_of(kind: JumpKind) -> PgResult<Cond> {
    raw::jump_to_cond(Jump::new(kind))
}

fn materialize_gp<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, reg: Reg, value: u64) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    raw::materialize_constant(machine, &[value], GP_BANK, 8, reg)
}

/// Unary op on one operand producing a result in a fresh (or reused) register.
fn unary<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: A64CompilerState,
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

fn binary<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, b: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: A64CompilerState,
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

/// Unary op whose result lives in another register bank than its operand
/// (a vector lane to a GP register, a GP scalar splat into a vector): a
/// fresh result register, never the operand's.
fn unary_fresh<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: A64CompilerState,
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

/// Binary op whose result must not alias an operand (needs both live after
/// the first emitted instruction).
fn binary_fresh<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, a: GValue, b: GValue, result: GValue, emit: impl FnOnce(&M, Reg, Reg, Reg) -> PgResult<()>) -> PgResult<()>
where
    M::Target: A64CompilerState,
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

fn compile_op<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, inst: GInst, op: &Op, result: Option<GValue>) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + A64CompilerState<Frontend = GravityFrontend>,
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
                    let mut tmp = ScratchReg::new(machine);
                    let t = tmp.alloc_gp()?;
                    materialize_gp(machine, t, bits)?;
                    raw::emit(machine, if ty == Type::F64 { de64_FMOVdx(raw::vector(reg)?, raw::zr(t)?) } else { de64_FMOVsw(raw::vector(reg)?, raw::zr(t)?) })?;
                }
                _ => {
                    let v = gravity_interp_canon(ty, bits);
                    materialize_gp(machine, reg, v)?;
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::IntBin { op: bop, a, b } => {
            let ty = ty_of(machine, g(a));
            // A no-wrap result already fits its type: canonicalizing as I64
            // is a no-op, so pass that as the canon type.
            let view = &machine.adaptor().view;
            let cty = if view.nowrap.get(result.unwrap().0 as usize).copied().unwrap_or(false) || view.no_canon.get(inst.0 as usize).copied().unwrap_or(false) { Type::I64 } else { ty };
            if let Some(&(other, x, k)) = view.shifted.get(&inst.0) {
                // `op other, (shl x k)` as the shifted-register form
                // (`FnView::fuse_narrow_ops`; 64-bit only, no canon).
                binary(machine, GValue(other), GValue(x), result.unwrap(), |m, rd, rn, rm| {
                    let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
                    raw::emit(
                        m,
                        match bop {
                            IntBinOp::Or => de64_ORRx_lsl(d, n, r, k),
                            IntBinOp::And => de64_ANDx_lsl(d, n, r, k),
                            IntBinOp::Xor => de64_EORx_lsl(d, n, r, k),
                            IntBinOp::Add => de64_ADDx_lsl(d, n, r, k),
                            IntBinOp::Sub => de64_SUBx_lsl(d, n, r, k),
                            _ => unreachable!(),
                        },
                    )
                })?;
                return Ok(true);
            }
            if let Some(cv) = fold_operand(machine, op)? {
                let (cty, cbits) = view.const_of[cv.index()].unwrap();
                let k = crate::view::const_canon(cty, cbits);
                // The register operand is whichever side is not the folded
                // constant (a commutative op may have it on the left).
                let src = if cv == b { a } else { b };
                unary(machine, g(src), result.unwrap(), |m, rd, rn| {
                    let (d, n) = (raw::zr(rd)?, raw::zr(rn)?);
                    let w = bits(ty);
                    match bop {
                        IntBinOp::Add if k >= 0 => raw::emit(m, de64_ADDxi(raw::sp(rd)?, raw::sp(rn)?, k))?,
                        IntBinOp::Add => raw::emit(m, de64_SUBxi(raw::sp(rd)?, raw::sp(rn)?, -k))?,
                        IntBinOp::Sub if k >= 0 => raw::emit(m, de64_SUBxi(raw::sp(rd)?, raw::sp(rn)?, k))?,
                        IntBinOp::Sub => raw::emit(m, de64_ADDxi(raw::sp(rd)?, raw::sp(rn)?, -k))?,
                        IntBinOp::And => raw::emit(m, de64_ANDxi(raw::sp(rd)?, n, k as u64))?,
                        IntBinOp::Or => raw::emit(m, de64_ORRxi(raw::sp(rd)?, n, k as u64))?,
                        IntBinOp::Xor => raw::emit(m, de64_EORxi(raw::sp(rd)?, n, k as u64))?,
                        IntBinOp::Shl => raw::emit(m, de64_LSLxi(d, n, k as u32))?,
                        IntBinOp::Ushr => {
                            if w < 64 {
                                zext_into(m, rd, rn, ty)?;
                                raw::emit(m, de64_LSRxi(d, d, k as u32))?
                            } else {
                                raw::emit(m, de64_LSRxi(d, n, k as u32))?
                            }
                        }
                        IntBinOp::Sshr => raw::emit(m, de64_ASRxi(d, n, k as u32))?,
                        // I64 only (verify): AArch64 rotates right only
                        IntBinOp::Rotl => raw::emit(m, de64_RORxi(d, n, (64 - k as u32) & 63))?,
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
            let view = &machine.adaptor().view;
            if view.icmp_flags.get(inst.0 as usize).copied().unwrap_or(false) {
                // The flags are the previous instruction's (`FnView::fuse_flags`).
                let r_ref = machine.result_ref(result.unwrap())?;
                let mut rp = r_ref.part(0);
                let rd = rp.alloc_reg()?;
                raw::emit(machine, de64_CSETx(raw::zr(rd)?, cond_of(icond(cond))?))?;
                rp.set_modified();
                return Ok(true);
            }
            if let Some(&w) = view.icmp_self_ext.get(&inst.0) {
                // `icmp eq/ne a, trunc(a)`: compare `a` with its own sign
                // extension from `w` bits (`FnView::fuse_narrow_ops`).
                unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                    let (n, r) = (raw::sp(rn)?, raw::zr(rn)?);
                    raw::emit(
                        m,
                        match w {
                            8 => de64_CMPx_sxtb(n, r, 0),
                            16 => de64_CMPx_sxth(n, r, 0),
                            _ => de64_CMPx_sxtw(n, r, 0),
                        },
                    )?;
                    raw::emit(m, de64_CSETx(raw::zr(rd)?, cond_of(icond(cond))?))
                })?;
                return Ok(true);
            }
            if let Some(cv) = fold_operand(machine, op)? {
                let (cty, cbits) = view.const_of[cv.index()].unwrap();
                let k = crate::view::const_canon(cty, cbits);
                unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                    if k >= 0 {
                        raw::emit(m, de64_CMPxi(raw::sp(rn)?, k))?;
                    } else {
                        raw::emit(m, de64_CMNxi(raw::sp(rn)?, -k))?;
                    }
                    raw::emit(m, de64_CSETx(raw::zr(rd)?, cond_of(icond(cond))?))
                })?;
                return Ok(true);
            }
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                raw::emit(m, de64_CMPx(raw::zr(rn)?, raw::zr(rm)?))?;
                raw::emit(m, de64_CSETx(raw::zr(rd)?, cond_of(icond(cond))?))
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
            let d = ty == Type::F64;
            unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                let (rd, rn) = (raw::vector(rd)?, raw::vector(rn)?);
                raw::emit(
                    m,
                    match (op, d) {
                        (FloatUnOp::Neg, true) => de64_FNEGd(rd, rn),
                        (FloatUnOp::Neg, false) => de64_FNEGs(rd, rn),
                        (FloatUnOp::Abs, true) => de64_FABSd(rd, rn),
                        (FloatUnOp::Abs, false) => de64_FABSs(rd, rn),
                        (FloatUnOp::Sqrt, true) => de64_FSQRTd(rd, rn),
                        (FloatUnOp::Sqrt, false) => de64_FSQRTs(rd, rn),
                        (FloatUnOp::Ceil, true) => de64_FRINTPd(rd, rn),
                        (FloatUnOp::Ceil, false) => de64_FRINTPs(rd, rn),
                        (FloatUnOp::Floor, true) => de64_FRINTMd(rd, rn),
                        (FloatUnOp::Floor, false) => de64_FRINTMs(rd, rn),
                        (FloatUnOp::Trunc, true) => de64_FRINTZd(rd, rn),
                        (FloatUnOp::Trunc, false) => de64_FRINTZs(rd, rn),
                        (FloatUnOp::Nearest, true) => de64_FRINTNd(rd, rn),
                        (FloatUnOp::Nearest, false) => de64_FRINTNs(rd, rn),
                    },
                )
            })?;
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
            let (ra, rb) = (raw::vector(ap.load_to_reg()?)?, raw::vector(bp.load_to_reg()?)?);
            let rd = rp.alloc_reg()?;
            raw::emit(machine, if d { de64_FCMP_d(ra, rb) } else { de64_FCMP_s(ra, rb) })?;
            raw::emit(machine, de64_CSETx(raw::zr(rd)?, cond_of(fcond(cond))?))?;
            rp.set_modified();
            Ok(true)
        }
        Op::Conv { op, to, a } => {
            if let Some(&(x, k)) = machine.adaptor().view.trunc_sbfx.get(&inst.0) {
                // `trunc (ushr x k)` (`FnView::fuse_narrow_ops`): the field
                // sign-extended in one instruction.
                let w = to.bytes() * 8;
                unary(machine, GValue(x), result.unwrap(), |m, rd, rn| raw::emit(m, de64_SBFXx(raw::zr(rd)?, raw::zr(rn)?, k, w)))?;
                return Ok(true);
            }
            let from = ty_of(machine, g(a));
            if (from.is_vector() || to.is_vector()) && !(op == ConvOp::Bitcast && from.is_vector() && to.is_vector()) {
                return Err(invalid("gravity_tpde: a vector conversion other than a vector bitcast is not encoded"));
            }
            compile_conv(machine, op, from, to, g(a), result.unwrap())?;
            Ok(true)
        }
        Op::Select { cond, a, b } => {
            let ty = ty_of(machine, g(a));
            {
                let c_ref = machine.value_ref(g(cond))?;
                let mut cp = c_ref.part(0);
                let c = cp.load_to_reg()?;
                raw::emit(machine, de64_CMPxi(raw::sp(c)?, 0))?;
            }
            let ne = cond_of(JumpKind::Ne)?;
            let eq = cond_of(JumpKind::Eq)?;
            if let Some(zv) = fold_operand(machine, op)? {
                // One operand is the constant 0: use xzr.
                let (other, c) = if zv == a { (g(b), eq) } else { (g(a), ne) };
                unary(machine, other, result.unwrap(), |m, rd, rn| raw::emit(m, de64_CSELx(raw::zr(rd)?, raw::zr(rn)?, DA_ZR, c)))?;
                return Ok(true);
            }
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                raw::emit(
                    m,
                    match ty {
                        Type::F64 => de64_FCSELd(raw::vector(rd)?, raw::vector(rn)?, raw::vector(rm)?, ne),
                        Type::F32 => de64_FCSELs(raw::vector(rd)?, raw::vector(rn)?, raw::vector(rm)?, ne),
                        _ => de64_CSELx(raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?, ne),
                    },
                )
            })?;
            Ok(true)
        }
        Op::Load { ty, ptr, offset, .. } | Op::LoadZext { ty, ptr, offset, .. } => {
            let size = ty.bytes();
            let zext = matches!(op, Op::LoadZext { .. }) || machine.adaptor().view.load_zext.get(inst.0 as usize).copied().unwrap_or(false);
            let sext = ty.is_int() && size < 8 && !zext;
            let (ptr, offset) = match machine.adaptor().view.load_override.get(&inst.0) {
                Some(&(base, Some((index, shift)), _)) => {
                    // Register-offset form (`FnView::fuse_addresses`).
                    let b_ref = machine.value_ref(GValue(base))?;
                    let mut bp = b_ref.part(0);
                    let i_ref = machine.value_ref(GValue(index))?;
                    let mut ip = i_ref.part(0);
                    let r_ref = machine.result_ref(result.unwrap())?;
                    let mut rp = r_ref.part(0);
                    let b = bp.load_to_reg()?;
                    let i = ip.load_to_reg()?;
                    let dst = rp.alloc_reg()?;
                    let (t, n, m) = (raw::zr(dst)?, raw::sp(b)?, raw::zr(i)?);
                    let sc = shift != 0;
                    let w = match (size, sext) {
                        (1, false) => de64_LDRBr(t, n, m),
                        (1, true) => de64_LDRSBxr(t, n, m),
                        (2, false) => de64_LDRHr_lsl(t, n, m, sc),
                        (2, true) => de64_LDRSHxr_lsl(t, n, m, sc),
                        (4, false) => de64_LDRwr_lsl(t, n, m, sc),
                        (4, true) => de64_LDRSWxr_lsl(t, n, m, sc),
                        _ => de64_LDRxr_lsl(t, n, m, sc),
                    };
                    raw::emit(machine, w)?;
                    rp.set_modified();
                    return Ok(true);
                }
                Some(&(base, None, off)) => (GValue(base), off as i32),
                None => (g(ptr), offset),
            };
            let p_ref = machine.value_ref(ptr)?;
            let mut pp = p_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let base = pp.load_to_reg()?;
            let dst = rp.alloc_reg()?;
            let (base, off) = if offset >= 0 {
                (base, offset as u32)
            } else {
                let mut tmp = ScratchReg::new(machine);
                let t = tmp.alloc_gp()?;
                add_imm(machine, t, base, offset as i64)?;
                (t, 0)
            };
            // TPDE's helper sign-extends 1/2-byte loads to 32 bits only; the
            // canonical form is 64-bit, so use the x-form encoders directly.
            let direct = if sext && size < 4 && off % size == 0 && off / size < 4096 {
                let w = if size == 1 { de64_LDRSBxu(raw::zr(dst)?, raw::sp(base)?, off as u64) } else { de64_LDRSHxu(raw::zr(dst)?, raw::sp(base)?, off as u64) };
                w != 0
            } else {
                false
            };
            if direct {
                let w = if size == 1 { de64_LDRSBxu(raw::zr(dst)?, raw::sp(base)?, off as u64) } else { de64_LDRSHxu(raw::zr(dst)?, raw::sp(base)?, off as u64) };
                raw::emit(machine, w)?;
            } else {
                raw::load_off(machine, dst, base, off, size, sext)?;
                if sext && size < 4 {
                    canon(machine, dst, ty)?;
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::Store { ptr, offset, value, .. } => {
            let view = &machine.adaptor().view;
            if let Some(&(base, index, shift)) = view.store_override.get(&inst.0) {
                // Register-offset form (`FnView::fuse_addresses`): a fused
                // zero constant is the zero register.
                let ty = view.store_zero_wide.get(&inst.0).copied().unwrap_or_else(|| ty_of(machine, g(value)));
                let zero = fold_operand(machine, op)?.is_some();
                let b_ref = machine.value_ref(GValue(base))?;
                let mut bp = b_ref.part(0);
                let i_ref = machine.value_ref(GValue(index))?;
                let mut ip = i_ref.part(0);
                let v_ref = if zero { None } else { Some(machine.value_ref(g(value))?) };
                let mut vp = v_ref.as_ref().map(|r| r.part(0));
                let n = raw::sp(bp.load_to_reg()?)?;
                let m = raw::zr(ip.load_to_reg()?)?;
                let t = match vp.as_mut() {
                    Some(p) => raw::zr(p.load_to_reg()?)?,
                    None => DA_ZR,
                };
                let sc = shift != 0;
                let w = match ty.bytes() {
                    1 => de64_STRBr(t, n, m),
                    2 => de64_STRHr_lsl(t, n, m, sc),
                    4 => de64_STRwr_lsl(t, n, m, sc),
                    _ => de64_STRxr_lsl(t, n, m, sc),
                };
                raw::emit(machine, w)?;
                return Ok(true);
            }
            // Two 8-byte stores to consecutive cells (`FnView::fuse_stores`):
            // one `stp`; a fused zero constant on either side is `xzr`.
            if let Some(&v2) = view.store_pair.get(&inst.0) {
                let is_zero = |v: u32| view.const_fused.get(v as usize).copied().unwrap_or(false) && view.const_of[v as usize].is_some_and(|(t, b)| crate::view::const_canon(t, b) == 0);
                let p_ref = machine.value_ref(g(ptr))?;
                let mut pp = p_ref.part(0);
                let base = pp.load_to_reg()?;
                let v1_ref = if is_zero(value.0) { None } else { Some(machine.value_ref(g(value))?) };
                let mut v1p = v1_ref.as_ref().map(|r| r.part(0));
                let r1 = match v1p.as_mut() {
                    Some(p) => raw::zr(p.load_to_reg()?)?,
                    None => DA_ZR,
                };
                let v2_ref = if is_zero(v2) { None } else { Some(machine.value_ref(GValue(v2))?) };
                let mut v2p = v2_ref.as_ref().map(|r| r.part(0));
                let r2 = match v2p.as_mut() {
                    Some(p) => raw::zr(p.load_to_reg()?)?,
                    None => DA_ZR,
                };
                tpde_util::tpde_assert!(offset % 8 == 0 && (-512..=504).contains(&offset), "stp offset");
                raw::emit(machine, de64_STPx(r1, r2, raw::sp(base)?, offset as i64))?;
                return Ok(true);
            }
            let ty = view.store_zero_wide.get(&inst.0).copied().unwrap_or_else(|| ty_of(machine, g(value)));
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let zero = fold_operand(machine, op)?.is_some();
            tpde_util::tpde_assert!(zero || !view.store_zero_wide.contains_key(&inst.0), "widened zero store of a non-zero value");
            if zero {
                // The value is a fused constant zero: store the zero register.
                let base = pp.load_to_reg()?;
                let size = ty.bytes();
                let (base, off) = if offset >= 0 && offset as u32 % size == 0 && (offset as u32 / size) < 4096 {
                    (base, offset as u32)
                } else {
                    let mut tmp = ScratchReg::new(machine);
                    let t = tmp.alloc_gp()?;
                    add_imm(machine, t, base, offset as i64)?;
                    (t, 0)
                };
                let a = raw::sp(base)?;
                let w = match size {
                    1 => de64_STRBu(DA_ZR, a, off as u64),
                    2 => de64_STRHu(DA_ZR, a, off as u64),
                    4 => de64_STRwu(DA_ZR, a, off as u64),
                    _ => de64_STRxu(DA_ZR, a, off as u64),
                };
                raw::emit(machine, w)?;
                return Ok(true);
            }
            let v_ref = machine.value_ref(g(value))?;
            let mut vp = v_ref.part(0);
            let base = pp.load_to_reg()?;
            let src = vp.load_to_reg()?;
            if offset >= 0 {
                raw::store_off(machine, base, offset as u32, src, ty.bytes())?;
            } else {
                let mut tmp = ScratchReg::new(machine);
                let t = tmp.alloc_gp()?;
                add_imm(machine, t, base, offset as i64)?;
                raw::store_off(machine, t, 0, src, ty.bytes())?;
            }
            Ok(true)
        }
        Op::Prefetch { ptr, offset } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let base = pp.load_to_reg()?;
            let w = if offset >= 0 { de64_PRFMu(DA_PRF_PLDL1KEEP, raw::sp(base)?, offset as u64) } else { 0 };
            if w != 0 {
                raw::emit(machine, w)?;
            } else {
                let mut tmp = ScratchReg::new(machine);
                let t = tmp.alloc_gp()?;
                add_imm(machine, t, base, offset as i64)?;
                raw::emit(machine, de64_PRFMu(DA_PRF_PLDL1KEEP, raw::sp(t)?, 0))?;
            }
            Ok(true)
        }
        Op::Crc32c { acc, data } => {
            binary(machine, g(acc), g(data), result.unwrap(), |m, rd, rn, rm| raw::emit(m, de64_CRC32CX(raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?)))?;
            Ok(true)
        }
        Op::AddHi { alo, blo, ahi, bhi } => {
            // `cmn alo, blo` sets C to the low word's carry; `adc` adds it into the high words.
            let l_ref = machine.value_ref(g(alo))?;
            let mut lp = l_ref.part(0);
            let m_ref = machine.value_ref(g(blo))?;
            let mut mp = m_ref.part(0);
            let (rl, rm) = (lp.load_to_reg()?, mp.load_to_reg()?);
            binary(machine, g(ahi), g(bhi), result.unwrap(), |m, rd, rn, rr| {
                raw::emit(m, de64_CMNx(raw::zr(rl)?, raw::zr(rm)?))?;
                raw::emit(m, de64_ADCx(raw::zr(rd)?, raw::zr(rn)?, raw::zr(rr)?))
            })?;
            Ok(true)
        }
        Op::StackAddr { slot } => {
            let nv = machine.adaptor().view.num_values;
            unary(machine, GValue(nv + slot.0), result.unwrap(), |m, rd, rn| {
                if rd != rn {
                    raw::emit(m, de64_MOVx(raw::zr(rd)?, raw::zr(rn)?))
                } else {
                    Ok(())
                }
            })?;
            Ok(true)
        }
        Op::PtrAdd { base, index, scale, offset } => {
            let view = &machine.adaptor().view;
            if view.ptr_add_override.get(&inst.0).is_none() {
                if let Some(cv) = fold_operand(machine, op)? {
                    // Constant index: `add rd, rn, #(k * scale + offset)`.
                    let (cty, cbits) = view.const_of[cv.index()].unwrap();
                    let k = crate::view::const_canon(cty, cbits);
                    let total = k.wrapping_mul(scale as i64).wrapping_add(offset as i64);
                    unary(machine, g(base), result.unwrap(), |m, rd, rn| {
                        if total == 0 {
                            raw::emit(m, de64_MOVx(raw::zr(rd)?, raw::zr(rn)?))
                        } else {
                            add_imm(m, rd, rn, total)
                        }
                    })?;
                    return Ok(true);
                }
            }
            let (index, scale, offset) = match view.ptr_add_override.get(&inst.0) {
                Some(&(i, sc, off)) => (GValue(i), sc, off),
                None => (g(index), scale as u64, offset as i64),
            };
            binary_fresh(machine, g(base), index, result.unwrap(), |m, rd, rn, rm| {
                let (d, n, i) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
                if scale == 0 {
                    raw::emit(m, de64_MOVx(d, n))?;
                } else if scale.is_power_of_two() {
                    raw::emit(m, de64_ADDx_lsl(d, n, i, scale.trailing_zeros()))?;
                } else {
                    let mut tmp = ScratchReg::new(m);
                    let t = tmp.alloc_gp()?;
                    materialize_gp(m, t, scale as u64)?;
                    raw::emit(m, de64_MADDx(d, i, raw::zr(t)?, n))?;
                }
                if offset != 0 {
                    add_imm(m, rd, rd, offset as i64)?;
                }
                Ok(())
            })?;
            Ok(true)
        }
        Op::Call { callee, ref args } => {
            let sym = machine.function_symbol(callee.0);
            let ret_ty = machine.adaptor().program.funcs[callee.index()].returns.first().copied();
            compile_call(machine, sym, args, result, ret_ty)?;
            Ok(true)
        }
        Op::CallHost { callee, ref args } => {
            let a = machine.adaptor();
            let sym = machine.function_symbol(a.host_func_index(callee.0));
            let ret_ty = a.program.host_fns[callee.index()].ret;
            compile_call(machine, sym, args, result, ret_ty)?;
            Ok(true)
        }
        // simd.md item 2: NEON selection for these is the next commit of
        // the SIMD lane; until then a program with vector ops is refused
        // (the lowerer emits none: its vector paths are gated off).
        Op::VecLoad { .. } | Op::VecStore { .. } | Op::VecBin { .. } | Op::VecShift { .. } | Op::VecCmp { .. } | Op::VecSelect { .. } | Op::VecReduceAdd { .. } | Op::VecMovmask { .. } | Op::VecSplat { .. } | Op::VecExtract { .. } | Op::VecInsert { .. } | Op::VecWiden { .. } | Op::VecNarrow { .. } => compile_vec_op(machine, op, result),
    }
}

/// Same rule as `gravity_interp::canon`: narrow ints sign-extended, Bool 0/1.
fn gravity_interp_canon(ty: Type, bits: u64) -> u64 {
    match ty {
        Type::Bool => (bits & 1) as u64,
        Type::I8 => bits as u8 as i8 as i64 as u64,
        Type::I16 => bits as u16 as i16 as i64 as u64,
        Type::I32 => bits as u32 as i32 as i64 as u64,
        _ => bits,
    }
}

fn add_imm<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, dst: Reg, src: Reg, imm: i64) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    if imm == 0 {
        if dst != src {
            raw::emit(machine, de64_MOVx(raw::zr(dst)?, raw::zr(src)?))?;
        }
        return Ok(());
    }
    let (d, s) = (raw::sp(dst)?, raw::sp(src)?);
    if (0..4096).contains(&imm) {
        return raw::emit(machine, de64_ADDxi(d, s, imm));
    }
    if (-4095..0).contains(&imm) {
        return raw::emit(machine, de64_SUBxi(d, s, -imm));
    }
    let mut tmp = ScratchReg::new(machine);
    let t = tmp.alloc_gp()?;
    materialize_gp(machine, t, imm as u64)?;
    raw::emit(machine, de64_ADDx(raw::zr(dst)?, raw::zr(src)?, raw::zr(t)?))
}

fn compile_call<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, sym: SymRef, args: &[gravity_ir::Value], result: Option<GValue>, ret_ty: Option<Type>) -> PgResult<()>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + A64CompilerState<Frontend = GravityFrontend>,
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
    M::Target: A64CompilerState,
{
    let w = bits(ty);
    match op {
        IntBinOp::Add | IntBinOp::Sub | IntBinOp::Mul | IntBinOp::And | IntBinOp::Or | IntBinOp::Xor | IntBinOp::Sdiv => binary(machine, a, b, result, |m, rd, rn, rm| {
            let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
            raw::emit(
                m,
                match op {
                    IntBinOp::Add => de64_ADDx(d, n, r),
                    IntBinOp::Sub => de64_SUBx(d, n, r),
                    IntBinOp::Mul => de64_MULx(d, n, r),
                    IntBinOp::And => de64_ANDx(d, n, r),
                    IntBinOp::Or => de64_ORRx(d, n, r),
                    IntBinOp::Xor => de64_EORx(d, n, r),
                    _ => de64_SDIVx(d, n, r),
                },
            )?;
            // Bool operands are 0/1, so and/or/xor results already are.
            if ty == Type::Bool { Ok(()) } else { canon(m, rd, cty) }
        }),
        // canonical (sign-extended) operands order the same way at 64 bits, signed and unsigned
        IntBinOp::Smin | IntBinOp::Smax | IntBinOp::Umin | IntBinOp::Umax => binary(machine, a, b, result, |m, rd, rn, rm| {
            let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
            raw::emit(m, de64_CMPx(n, r))?;
            let take_n = match op {
                IntBinOp::Smin => JumpKind::Lt,
                IntBinOp::Smax => JumpKind::Gt,
                IntBinOp::Umin => JumpKind::Cc,
                _ => JumpKind::Hi,
            };
            raw::emit(m, de64_CSELx(d, n, r, cond_of(take_n)?))
        }),
        // I64 only (verify): the high word needs no canon
        IntBinOp::Smulh => binary(machine, a, b, result, |m, rd, rn, rm| raw::emit(m, de64_SMULH(raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?))),
        IntBinOp::Umulh => binary(machine, a, b, result, |m, rd, rn, rm| raw::emit(m, de64_UMULH(raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?))),
        // I64 only (verify): a left rotate by `r` is a right rotate by `-r` (RORV takes the count modulo 64)
        IntBinOp::Rotl => binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
            let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
            raw::emit(m, de64_NEGx(d, r))?;
            raw::emit(m, de64_RORVx(d, n, d))
        }),
        IntBinOp::Srem => binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
            let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
            raw::emit(m, de64_SDIVx(d, n, r))?;
            raw::emit(m, de64_MSUBx(d, d, r, n))?;
            canon(m, rd, cty)
        }),
        IntBinOp::Udiv | IntBinOp::Urem => binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
            let mut ta = ScratchReg::new(m);
            let mut tb = ScratchReg::new(m);
            let (n, r) = if w < 64 {
                let n = ta.alloc_gp()?;
                let r = tb.alloc_gp()?;
                zext_into(m, n, rn, ty)?;
                zext_into(m, r, rm, ty)?;
                (n, r)
            } else {
                (rn, rm)
            };
            let (d, n, r) = (raw::zr(rd)?, raw::zr(n)?, raw::zr(r)?);
            raw::emit(m, de64_UDIVx(d, n, r))?;
            if op == IntBinOp::Urem {
                raw::emit(m, de64_MSUBx(d, d, r, n))?;
            }
            canon(m, rd, cty)
        }),
        IntBinOp::Shl | IntBinOp::Ushr | IntBinOp::Sshr => binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
            let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
            match w {
                64 => raw::emit(
                    m,
                    match op {
                        IntBinOp::Shl => de64_LSLVx(d, n, r),
                        IntBinOp::Ushr => de64_LSRVx(d, n, r),
                        _ => de64_ASRVx(d, n, r),
                    },
                ),
                32 => {
                    raw::emit(
                        m,
                        match op {
                            IntBinOp::Shl => de64_LSLVw(d, n, r),
                            IntBinOp::Ushr => de64_LSRVw(d, n, r),
                            _ => de64_ASRVw(d, n, r),
                        },
                    )?;
                    canon(m, rd, cty)
                }
                _ => {
                    let mut tc = ScratchReg::new(m);
                    let c = tc.alloc_gp()?;
                    raw::emit(m, de64_ANDxi(raw::sp(c)?, r, (w - 1) as u64))?;
                    let c = raw::zr(c)?;
                    match op {
                        IntBinOp::Shl => raw::emit(m, de64_LSLVx(d, n, c))?,
                        IntBinOp::Ushr => {
                            zext_into(m, rd, rn, ty)?;
                            raw::emit(m, de64_LSRVx(d, d, c))?
                        }
                        _ => raw::emit(m, de64_ASRVx(d, n, c))?,
                    }
                    canon(m, rd, cty)
                }
            }
        }),
    }
}

fn compile_int_ovf<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: IntOvfOp, ty: Type, a: GValue, b: GValue, result: GValue) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let w = bits(ty);
    let vs = cond_of(JumpKind::Vs)?;
    let ne = cond_of(JumpKind::Ne)?;
    binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
        let (d, n, r) = (raw::zr(rd)?, raw::zr(rn)?, raw::zr(rm)?);
        let mut tmp = ScratchReg::new(m);
        let t = tmp.alloc_gp()?;
        let tz = raw::zr(t)?;
        match (op, w) {
            (IntOvfOp::SaddOvf, 64) => {
                raw::emit(m, de64_ADDSx(tz, n, r))?;
                raw::emit(m, de64_CSETx(d, vs))
            }
            (IntOvfOp::SsubOvf, 64) => {
                raw::emit(m, de64_SUBSx(tz, n, r))?;
                raw::emit(m, de64_CSETx(d, vs))
            }
            (IntOvfOp::SaddOvf, 32) => {
                raw::emit(m, de64_ADDSw(tz, n, r))?;
                raw::emit(m, de64_CSETx(d, vs))
            }
            (IntOvfOp::SsubOvf, 32) => {
                raw::emit(m, de64_SUBSw(tz, n, r))?;
                raw::emit(m, de64_CSETx(d, vs))
            }
            (IntOvfOp::SmulOvf, 64) => {
                raw::emit(m, de64_MULx(tz, n, r))?;
                raw::emit(m, de64_SMULH(d, n, r))?;
                raw::emit(m, de64_CMPx_asr(d, tz, 63))?;
                raw::emit(m, de64_CSETx(d, ne))
            }
            _ => {
                // Exact 64-bit result of sign-extended operands, compared with
                // its truncation to the type.
                raw::emit(
                    m,
                    match op {
                        IntOvfOp::SaddOvf => de64_ADDx(tz, n, r),
                        IntOvfOp::SsubOvf => de64_SUBx(tz, n, r),
                        IntOvfOp::SmulOvf => de64_MULx(tz, n, r),
                    },
                )?;
                raw::emit(m, de64_MOVx(d, tz))?;
                canon(m, rd, ty)?;
                raw::emit(m, de64_CMPx(d, tz))?;
                raw::emit(m, de64_CSETx(d, ne))
            }
        }
    })
}

fn compile_int_un<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: IntUnOp, ty: Type, a: GValue, result: GValue) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let w = bits(ty);
    unary(machine, a, result, |m, rd, rn| {
        let (d, n) = (raw::zr(rd)?, raw::zr(rn)?);
        match op {
            IntUnOp::Neg => {
                raw::emit(m, de64_NEGx(d, n))?;
                canon(m, rd, ty)
            }
            IntUnOp::Not => {
                if ty == Type::Bool {
                    raw::emit(m, de64_EORxi(raw::sp(rd)?, n, 1))
                } else {
                    raw::emit(m, de64_MVNx(d, n))?;
                    canon(m, rd, ty)
                }
            }
            IntUnOp::Clz => match w {
                64 => raw::emit(m, de64_CLZx(d, n)),
                32 => raw::emit(m, de64_CLZw(d, n)),
                _ => {
                    zext_into(m, rd, rn, ty)?;
                    raw::emit(m, de64_CLZw(d, d))?;
                    raw::emit(m, de64_SUBxi(raw::sp(rd)?, raw::sp(rd)?, (32 - w) as i64))
                }
            },
            IntUnOp::Ctz => {
                if w < 64 {
                    let mut tmp = ScratchReg::new(m);
                    let t = tmp.alloc_gp()?;
                    materialize_gp(m, t, 1u64 << w)?;
                    raw::emit(m, de64_ORRx(d, n, raw::zr(t)?))?;
                    raw::emit(m, de64_RBITx(d, d))?;
                } else {
                    raw::emit(m, de64_RBITx(d, n))?;
                }
                raw::emit(m, de64_CLZx(d, d))
            }
            IntUnOp::Popcnt => {
                zext_into(m, rd, rn, ty)?;
                let mut tmp = ScratchReg::new(m);
                let v = raw::vector(tmp.alloc(FP_BANK)?)?;
                raw::emit(m, de64_FMOVdx(v, d))?;
                raw::emit(m, de64_CNT8b(v, v))?;
                raw::emit(m, de64_ADDV8b(v, v))?;
                raw::emit(m, de64_FMOVxd(d, v))
            }
            IntUnOp::Bitrev => {
                // The sign-extension bits land low and the shift drops them.
                raw::emit(m, de64_RBITx(d, n))?;
                if w < 64 {
                    raw::emit(m, de64_LSRxi(d, d, 64 - w))?;
                    canon(m, rd, ty)
                } else {
                    Ok(())
                }
            }
            IntUnOp::Bswap => match w {
                64 => raw::emit(m, de64_REV64x(d, n)),
                32 => {
                    raw::emit(m, de64_REV32w(d, n))?;
                    canon(m, rd, ty)
                }
                16 => {
                    raw::emit(m, de64_REV16w(d, n))?;
                    canon(m, rd, ty)
                }
                _ => {
                    if rd != rn {
                        raw::emit(m, de64_MOVx(d, n))?;
                    }
                    Ok(())
                }
            },
        }
    })
}

fn compile_float_bin<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: FloatBinOp, ty: Type, a: GValue, b: GValue, result: GValue) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let d = ty == Type::F64;
    if op == FloatBinOp::Copysign {
        // (a & ~sign) | (b & sign) through GP registers.
        return binary_fresh(machine, a, b, result, |m, rd, rn, rm| {
            let (vd, vn, vm) = (raw::vector(rd)?, raw::vector(rn)?, raw::vector(rm)?);
            let mut t1 = ScratchReg::new(m);
            let mut t2 = ScratchReg::new(m);
            let x = t1.alloc_gp()?;
            let y = t2.alloc_gp()?;
            let (xz, yz) = (raw::zr(x)?, raw::zr(y)?);
            if d {
                raw::emit(m, de64_FMOVxd(xz, vn))?;
                raw::emit(m, de64_FMOVxd(yz, vm))?;
                raw::emit(m, de64_ANDxi(raw::sp(x)?, xz, 0x7fff_ffff_ffff_ffff))?;
                raw::emit(m, de64_ANDxi(raw::sp(y)?, yz, 0x8000_0000_0000_0000))?;
                raw::emit(m, de64_ORRx(xz, xz, yz))?;
                raw::emit(m, de64_FMOVdx(vd, xz))
            } else {
                raw::emit(m, de64_FMOVws(xz, vn))?;
                raw::emit(m, de64_FMOVws(yz, vm))?;
                raw::emit(m, de64_ANDwi(raw::sp(x)?, xz, 0x7fff_ffff))?;
                raw::emit(m, de64_ANDwi(raw::sp(y)?, yz, 0x8000_0000))?;
                raw::emit(m, de64_ORRw(xz, xz, yz))?;
                raw::emit(m, de64_FMOVsw(vd, xz))
            }
        });
    }
    binary(machine, a, b, result, |m, rd, rn, rm| {
        let (vd, vn, vm) = (raw::vector(rd)?, raw::vector(rn)?, raw::vector(rm)?);
        raw::emit(
            m,
            match (op, d) {
                (FloatBinOp::Add, true) => de64_FADDd(vd, vn, vm),
                (FloatBinOp::Add, false) => de64_FADDs(vd, vn, vm),
                (FloatBinOp::Sub, true) => de64_FSUBd(vd, vn, vm),
                (FloatBinOp::Sub, false) => de64_FSUBs(vd, vn, vm),
                (FloatBinOp::Mul, true) => de64_FMULd(vd, vn, vm),
                (FloatBinOp::Mul, false) => de64_FMULs(vd, vn, vm),
                (FloatBinOp::Div, true) => de64_FDIVd(vd, vn, vm),
                (FloatBinOp::Div, false) => de64_FDIVs(vd, vn, vm),
                // Rust f64::min/max: return the non-NaN operand (IEEE minNum).
                (FloatBinOp::Min, true) => de64_FMINNMd(vd, vn, vm),
                (FloatBinOp::Min, false) => de64_FMINNMs(vd, vn, vm),
                (FloatBinOp::Max, true) => de64_FMAXNMd(vd, vn, vm),
                (FloatBinOp::Max, false) => de64_FMAXNMs(vd, vn, vm),
                (FloatBinOp::Copysign, _) => unreachable!(),
            },
        )
    })
}

fn compile_conv<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: ConvOp, from: Type, to: Type, a: GValue, result: GValue) -> PgResult<()>
where
    M::Target: A64CompilerState,
{
    let same_bank = from.is_float() == to.is_float();
    let emit_mov = |m: &M, rd: Reg, rn: Reg| -> PgResult<()> {
        if rd != rn {
            if to.is_float() {
                raw::emit(m, if to == Type::F64 { de64_FMOVd(raw::vector(rd)?, raw::vector(rn)?) } else { de64_FMOVs(raw::vector(rd)?, raw::vector(rn)?) })
            } else {
                raw::emit(m, de64_MOVx(raw::zr(rd)?, raw::zr(rn)?))
            }
        } else {
            Ok(())
        }
    };
    let go = |f: &dyn Fn(&M, Reg, Reg) -> PgResult<()>| -> PgResult<()> {
        if same_bank {
            unary(machine, a, result, |m, rd, rn| f(m, rd, rn))
        } else {
            // Result bank differs from the operand bank: never reuse.
            let a_ref = machine.value_ref(a)?;
            let mut ap = a_ref.part(0);
            let r_ref = machine.result_ref(result)?;
            let mut rp = r_ref.part(0);
            let rn = ap.load_to_reg()?;
            let rd = rp.alloc_reg()?;
            f(machine, rd, rn)?;
            rp.set_modified();
            Ok(())
        }
    };
    match op {
        ConvOp::Sext | ConvOp::PtrToInt | ConvOp::IntToPtr => go(&emit_mov),
        ConvOp::Zext => go(&|m, rd, rn| zext_into(m, rd, rn, from)),
        ConvOp::Trunc => go(&|m, rd, rn| {
            emit_mov(m, rd, rn)?;
            canon(m, rd, to)
        }),
        ConvOp::SintToFloat => go(&|m, rd, rn| {
            let (v, x) = (raw::vector(rd)?, raw::zr(rn)?);
            raw::emit(m, if to == Type::F64 { de64_SCVTFdx(v, x) } else { de64_SCVTFsx(v, x) })
        }),
        ConvOp::UintToFloat => go(&|m, rd, rn| {
            let mut tmp = ScratchReg::new(m);
            let t = tmp.alloc_gp()?;
            zext_into(m, t, rn, from)?;
            let (v, x) = (raw::vector(rd)?, raw::zr(t)?);
            raw::emit(m, if to == Type::F64 { de64_UCVTFdx(v, x) } else { de64_UCVTFsx(v, x) })
        }),
        ConvOp::FloatToSint | ConvOp::FloatToUint => go(&|m, rd, rn| {
            let (x, v) = (raw::zr(rd)?, raw::vector(rn)?);
            raw::emit(
                m,
                match (op, from) {
                    (ConvOp::FloatToSint, Type::F64) => de64_FCVTZSxd(x, v),
                    (ConvOp::FloatToSint, _) => de64_FCVTZSxs(x, v),
                    (_, Type::F64) => de64_FCVTZUxd(x, v),
                    _ => de64_FCVTZUxs(x, v),
                },
            )?;
            canon(m, rd, to)
        }),
        ConvOp::Fpromote => go(&|m, rd, rn| raw::emit(m, de64_FCVTds(raw::vector(rd)?, raw::vector(rn)?))),
        ConvOp::Fdemote => go(&|m, rd, rn| raw::emit(m, de64_FCVTsd(raw::vector(rd)?, raw::vector(rn)?))),
        ConvOp::Bitcast => go(&|m, rd, rn| match (from, to) {
            (f, t) if f.is_vector() && t.is_vector() => {
                if rd != rn {
                    raw::emit(m, de64_ORR16b(raw::vector(rd)?, raw::vector(rn)?, raw::vector(rn)?))?;
                }
                Ok(())
            }
            (Type::F64, _) => raw::emit(m, de64_FMOVxd(raw::zr(rd)?, raw::vector(rn)?)),
            (Type::F32, _) => {
                raw::emit(m, de64_FMOVws(raw::zr(rd)?, raw::vector(rn)?))?;
                canon(m, rd, to)
            }
            (_, Type::F64) => raw::emit(m, de64_FMOVdx(raw::vector(rd)?, raw::zr(rn)?)),
            (_, Type::F32) => raw::emit(m, de64_FMOVsw(raw::vector(rd)?, raw::zr(rn)?)),
            _ => emit_mov(m, rd, rn),
        }),
    }
}

fn compile_term<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, block: u32) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + A64CompilerState<Frontend = GravityFrontend>,
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
            if let Some(f) = adaptor.view.term_fuse[block as usize] {
                let a_ref = machine.value_ref(GValue(f.a.0))?;
                let mut ap = a_ref.part(0);
                let ra = ap.load_to_reg()?;
                let mut jump = match (f.icond, f.b) {
                    (Some(c), None) if f.self_ext.is_some() => {
                        let (n, m) = (raw::sp(ra)?, raw::zr(ra)?);
                        raw::emit(
                            machine,
                            match f.self_ext.unwrap() {
                                8 => de64_CMPx_sxtb(n, m, 0),
                                16 => de64_CMPx_sxth(n, m, 0),
                                _ => de64_CMPx_sxtw(n, m, 0),
                            },
                        )?;
                        Jump::new(icond(c))
                    }
                    (None, Some(b)) if f.ovf.is_some() => {
                        let (op, w) = f.ovf.unwrap();
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        let mut tmp = ScratchReg::new(machine);
                        let t = raw::zr(tmp.alloc_gp()?)?;
                        let (n, m) = (raw::zr(ra)?, raw::zr(rb)?);
                        raw::emit(
                            machine,
                            match (op, w) {
                                (IntOvfOp::SaddOvf, 64) => de64_ADDSx(t, n, m),
                                (IntOvfOp::SsubOvf, 64) => de64_SUBSx(t, n, m),
                                (IntOvfOp::SaddOvf, _) => de64_ADDSw(t, n, m),
                                _ => de64_SUBSw(t, n, m),
                            },
                        )?;
                        Jump::new(JumpKind::Vs)
                    }
                    (Some(c @ (IntCond::Eq | IntCond::Ne)), None) if f.imm == 0 && f.second.is_none() => {
                        let kind = if c == IntCond::Eq { JumpKind::Cbz } else { JumpKind::Cbnz };
                        Jump { kind, cmp_reg: ra, cmp_is_32: false, test_bit: 0 }
                    }
                    (Some(c), None) => {
                        raw::emit(machine, de64_CMPxi(raw::sp(ra)?, f.imm as i64))?;
                        Jump::new(icond(c))
                    }
                    (Some(c), Some(b)) => {
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        raw::emit(machine, de64_CMPx(raw::zr(ra)?, raw::zr(rb)?))?;
                        Jump::new(icond(c))
                    }
                    (None, Some(b)) => {
                        let b_ref = machine.value_ref(GValue(b.0))?;
                        let mut bp = b_ref.part(0);
                        let rb = bp.load_to_reg()?;
                        raw::emit(machine, if f.is_f64 { de64_FCMP_d(raw::vector(ra)?, raw::vector(rb)?) } else { de64_FCMP_s(raw::vector(ra)?, raw::vector(rb)?) })?;
                        Jump::new(fcond(f.fcond.unwrap()))
                    }
                    (None, None) => unreachable!(),
                };
                for s in [f.second, f.third].into_iter().flatten() {
                    // `cmp; ccmp; b.cond_b` (`FnView::fuse_compares`): on
                    // `iand` the `ccmp` runs under the first condition and
                    // sets flags failing the second when it does not hold;
                    // on `ior` it runs under the first's inverse and sets
                    // flags passing the second. A third condition chains
                    // the same way under the second's.
                    let cond_a = raw::jump_to_cond(jump)?;
                    let cond_b = icond(s.icond);
                    let (under, nzcv) = if s.or {
                        (raw::jump_to_cond(raw::invert_jump(jump))?, nzcv_true(cond_b))
                    } else {
                        (cond_a, nzcv_true(raw::invert_jump(Jump::new(cond_b)).kind))
                    };
                    tpde_util::tpde_assert!(nzcv < 16, "ccmp nzcv immediate");
                    let s_ref = machine.value_ref(GValue(s.a.0))?;
                    let mut sp = s_ref.part(0);
                    let rs = sp.load_to_reg()?;
                    match s.b {
                        Some(b) => {
                            let b_ref = machine.value_ref(GValue(b.0))?;
                            let mut bp = b_ref.part(0);
                            let rb = bp.load_to_reg()?;
                            raw::emit(machine, de64_CCMPx(raw::zr(rs)?, raw::zr(rb)?, nzcv, under))?;
                        }
                        None => {
                            tpde_util::tpde_assert!(s.imm < 32, "ccmp imm5");
                            raw::emit(machine, de64_CCMPxi(raw::zr(rs)?, s.imm, nzcv, under))?;
                        }
                    }
                    jump = Jump::new(cond_b);
                }
                // A flag-based jump no longer needs the compare operand:
                // release it before the phi moves so an edge can coalesce
                // it (a `cbz`/`tbz` reads the register at the jump).
                if !matches!(jump.kind, JumpKind::Cbz | JumpKind::Cbnz | JumpKind::Tbz | JumpKind::Tbnz) {
                    drop(ap);
                    drop(a_ref);
                    branches::generate_cond_branch(machine, jump, GBlock(succs[0]), GBlock(succs[1]))?;
                } else {
                    // the jump reads the register: held across the branch, then released as the branch would have (D117)
                    branches::generate_cond_branch(machine, jump, GBlock(succs[0]), GBlock(succs[1]))?;
                    drop(ap);
                    drop(a_ref);
                    branches::release_jump_operand(machine, ra);
                }
            } else {
                let c_ref = machine.value_ref(GValue(cond.0))?;
                let mut cp = c_ref.part(0);
                let reg = cp.load_to_reg()?;
                let jump = Jump { kind: JumpKind::Cbnz, cmp_reg: reg, cmp_is_32: false, test_bit: 0 };
                branches::generate_cond_branch(machine, jump, GBlock(succs[0]), GBlock(succs[1]))?;
                drop(cp);
                drop(c_ref);
                branches::release_jump_operand(machine, reg);
            }
        }
        Terminator::Switch { value, cases, .. } => {
            let ty = ty_of(machine, GValue(value.0));
            let mut scratch = ScratchReg::new(machine);
            let sreg = scratch.alloc_gp()?;
            {
                let v_ref = machine.value_ref(GValue(value.0))?;
                let mut vp = v_ref.part(0);
                let reg = vp.load_to_reg()?;
                raw::emit(machine, de64_MOVx(raw::zr(sreg)?, raw::zr(reg)?))?;
            }
            let table: mcx::PgVec<(u64, GBlock)> = mcx::CollectIn::collect_in(cases.iter().enumerate().map(|(i, c)| (c.value, GBlock(succs[i]))), adaptor.view.mcx());
            branches::generate_switch(machine, scratch, bits(ty), GBlock(succs[cases.len()]), &table)?;
        }
        Terminator::Return(values) => {
            if values.len() > 8 {
                return Err(invalid("gravity_tpde: more than eight return values"));
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
            raw::emit(machine, de64_BRK(1))?;
            machine.release_regs_after_return();
        }
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// Vector ops (docs/design/gravity-simple/simd.md §2): one NEON instruction
// or a short fixed sequence per op; 16-byte values live in `q` registers
// (`Parts { size: 16, bank: FP_BANK }`, spilled as `ldr q` / `str q`).
// ---------------------------------------------------------------------------

/// The element index of a `d`/`s`/`h`/`b` lane as the encoders take it
/// (`immvidx`: the index, the size bit is the form's).
fn lane_bytes(lane: Type) -> u32 {
    match lane {
        Type::I64 | Type::F64 => 8,
        Type::I32 | Type::F32 => 4,
        Type::I16 => 2,
        _ => 1,
    }
}

fn compile_vec_op<'m, 'p, M: TargetMachine<'m, A<'p>, 2>>(machine: &M, op: &Op, result: Option<GValue>) -> PgResult<bool>
where
    M::Target: CompilerTarget<A<'p>, 2, Jump = Jump> + A64CompilerState<Frontend = GravityFrontend>,
{
    let g = |v: gravity_ir::Value| GValue(v.0);
    let v = |r: Reg| raw::vector(r);
    match *op {
        Op::VecLoad { ptr, offset, .. } => {
            let p_ref = machine.value_ref(g(ptr))?;
            let mut pp = p_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let base = pp.load_to_reg()?;
            let dst = rp.alloc_reg()?;
            if offset >= 0 && offset % 16 == 0 {
                raw::load_off(machine, dst, base, offset as u32, 16, false)?;
            } else if (-256..256).contains(&offset) {
                raw::emit(machine, de64_LDURq(v(dst)?, raw::sp(base)?, offset as i64))?;
            } else {
                let mut tmp = ScratchReg::new(machine);
                let t = tmp.alloc_gp()?;
                add_imm(machine, t, base, offset as i64)?;
                raw::emit(machine, de64_LDRqu(v(dst)?, raw::sp(t)?, 0))?;
            }
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
            if offset >= 0 && offset % 16 == 0 {
                raw::store_off(machine, base, offset as u32, src, 16)?;
            } else if (-256..256).contains(&offset) {
                raw::emit(machine, de64_STURq(v(src)?, raw::sp(base)?, offset as i64))?;
            } else {
                let mut tmp = ScratchReg::new(machine);
                let t = tmp.alloc_gp()?;
                add_imm(machine, t, base, offset as i64)?;
                raw::emit(machine, de64_STRqu(v(src)?, raw::sp(t)?, 0))?;
            }
            Ok(true)
        }
        Op::VecBin { op: bop @ (VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax), a, b } if ty_of(machine, g(a)).lane() == Type::I64 => {
            // no NEON min/max over 64-bit lanes: `cmgt` into the fresh result, then `bsl`
            binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let (d, n, r) = (v(rd)?, v(rn)?, v(rm)?);
                let (signed, take_n_when_greater) = match bop {
                    VecBinOp::Smin => (true, false),
                    VecBinOp::Smax => (true, true),
                    VecBinOp::Umin => (false, false),
                    _ => (false, true),
                };
                let (x, y) = if take_n_when_greater { (n, r) } else { (r, n) };
                raw::emit(m, if signed { de64_CMGT2d(d, x, y) } else { de64_CMHI2d(d, x, y) })?;
                raw::emit(m, de64_BSL16b(d, n, r))
            })?;
            Ok(true)
        }
        Op::VecBin { op: bop, a, b } => {
            let ty = ty_of(machine, g(a));
            let lane = ty.lane();
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let (d, n, r) = (v(rd)?, v(rn)?, v(rm)?);
                let w = match (bop, lane) {
                    (VecBinOp::And, _) => de64_AND16b(d, n, r),
                    (VecBinOp::Or, _) => de64_ORR16b(d, n, r),
                    (VecBinOp::Xor, _) => de64_EOR16b(d, n, r),
                    (VecBinOp::Add, Type::I64) => de64_ADD2d(d, n, r),
                    (VecBinOp::Add, Type::I32) => de64_ADD4s(d, n, r),
                    (VecBinOp::Add, Type::I16) => de64_ADD8h(d, n, r),
                    (VecBinOp::Add, Type::I8) => de64_ADD16b(d, n, r),
                    (VecBinOp::Add, Type::F64) => de64_FADD2d(d, n, r),
                    (VecBinOp::Add, Type::F32) => de64_FADD4s(d, n, r),
                    (VecBinOp::Sub, Type::I64) => de64_SUB2d(d, n, r),
                    (VecBinOp::Sub, Type::I32) => de64_SUB4s(d, n, r),
                    (VecBinOp::Sub, Type::I16) => de64_SUB8h(d, n, r),
                    (VecBinOp::Sub, Type::I8) => de64_SUB16b(d, n, r),
                    (VecBinOp::Sub, Type::F64) => de64_FSUB2d(d, n, r),
                    (VecBinOp::Sub, Type::F32) => de64_FSUB4s(d, n, r),
                    (VecBinOp::Mul, Type::I32) => de64_MUL4s(d, n, r),
                    (VecBinOp::Mul, Type::I16) => de64_MUL8h(d, n, r),
                    (VecBinOp::Mul, Type::I8) => de64_MUL16b(d, n, r),
                    (VecBinOp::Mul, Type::F64) => de64_FMUL2d(d, n, r),
                    (VecBinOp::Mul, Type::F32) => de64_FMUL4s(d, n, r),
                    (VecBinOp::Smin | VecBinOp::Smax | VecBinOp::Umin | VecBinOp::Umax, Type::I8 | Type::I16 | Type::I32) => neon_minmax(bop, lane, rd, rn, rm)?,
                    _ => return Err(invalid("gravity_tpde: vector op has no NEON form for this lane type")),
                };
                raw::emit(m, w)
            })?;
            Ok(true)
        }
        Op::VecShift { op: sop, a, count } => {
            let lane = ty_of(machine, g(a)).lane();
            unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                let (d, n) = (v(rd)?, v(rn)?);
                let k = count as u32;
                if k == 0 {
                    // `ushr` / `sshr` take 1..width: a shift by 0 is the value
                    return if rd != rn { raw::emit(m, de64_ORR16b(d, n, n)) } else { Ok(()) };
                }
                let w = match (sop, lane) {
                    (VecShiftOp::Shl, Type::I64) => de64_SHL2di(d, n, k),
                    (VecShiftOp::Shl, Type::I32) => de64_SHL4si(d, n, k),
                    (VecShiftOp::Shl, Type::I16) => de64_SHL8hi(d, n, k),
                    (VecShiftOp::Shl, Type::I8) => de64_SHL16bi(d, n, k),
                    (VecShiftOp::Ushr, Type::I64) => de64_USHR2di(d, n, k),
                    (VecShiftOp::Ushr, Type::I32) => de64_USHR4si(d, n, k),
                    (VecShiftOp::Ushr, Type::I16) => de64_USHR8hi(d, n, k),
                    (VecShiftOp::Ushr, Type::I8) => de64_USHR16bi(d, n, k),
                    (VecShiftOp::Sshr, Type::I64) => de64_SSHR2di(d, n, k),
                    (VecShiftOp::Sshr, Type::I32) => de64_SSHR4si(d, n, k),
                    (VecShiftOp::Sshr, Type::I16) => de64_SSHR8hi(d, n, k),
                    (VecShiftOp::Sshr, Type::I8) => de64_SSHR16bi(d, n, k),
                    _ => return Err(invalid("gravity_tpde: vector shift on a float lane")),
                };
                raw::emit(m, w)
            })?;
            Ok(true)
        }
        Op::VecCmp { cond, a, b } => {
            let lane = ty_of(machine, g(a)).lane();
            // `lt`/`le`/`ult`/`ule` by swapping the operands; `ne` = `eq` + `not`.
            let (a, b, cond) = match cond {
                IntCond::Slt => (b, a, IntCond::Sgt),
                IntCond::Sle => (b, a, IntCond::Sge),
                IntCond::Ult => (b, a, IntCond::Ugt),
                IntCond::Ule => (b, a, IntCond::Uge),
                c => (a, b, c),
            };
            binary(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let (d, n, r) = (v(rd)?, v(rn)?, v(rm)?);
                let w = match (cond, lane) {
                    (IntCond::Eq | IntCond::Ne, Type::I64) => de64_CMEQ2d(d, n, r),
                    (IntCond::Eq | IntCond::Ne, Type::I32) => de64_CMEQ4s(d, n, r),
                    (IntCond::Eq | IntCond::Ne, Type::I16) => de64_CMEQ8h(d, n, r),
                    (IntCond::Eq | IntCond::Ne, Type::I8) => de64_CMEQ16b(d, n, r),
                    (IntCond::Sgt, Type::I64) => de64_CMGT2d(d, n, r),
                    (IntCond::Sgt, Type::I32) => de64_CMGT4s(d, n, r),
                    (IntCond::Sgt, Type::I16) => de64_CMGT8h(d, n, r),
                    (IntCond::Sgt, Type::I8) => de64_CMGT16b(d, n, r),
                    (IntCond::Sge, Type::I64) => de64_CMGE2d(d, n, r),
                    (IntCond::Sge, Type::I32) => de64_CMGE4s(d, n, r),
                    (IntCond::Sge, Type::I16) => de64_CMGE8h(d, n, r),
                    (IntCond::Sge, Type::I8) => de64_CMGE16b(d, n, r),
                    (IntCond::Ugt, Type::I64) => de64_CMHI2d(d, n, r),
                    (IntCond::Ugt, Type::I32) => de64_CMHI4s(d, n, r),
                    (IntCond::Ugt, Type::I16) => de64_CMHI8h(d, n, r),
                    (IntCond::Ugt, Type::I8) => de64_CMHI16b(d, n, r),
                    (IntCond::Uge, Type::I64) => de64_CMHS2d(d, n, r),
                    (IntCond::Uge, Type::I32) => de64_CMHS4s(d, n, r),
                    (IntCond::Uge, Type::I16) => de64_CMHS8h(d, n, r),
                    (IntCond::Uge, Type::I8) => de64_CMHS16b(d, n, r),
                    _ => return Err(invalid("gravity_tpde: vector compare on a float lane")),
                };
                raw::emit(m, w)?;
                if cond == IntCond::Ne {
                    raw::emit(m, de64_NOT16b(d, d))?;
                }
                Ok(())
            })?;
            Ok(true)
        }
        Op::VecSelect { mask, a, b } => {
            // `bsl rd, a, b` reads rd as the mask: rd is the mask's copy,
            // and must not alias `a` / `b` (a fresh register).
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
            if rd != mr {
                raw::emit(machine, de64_ORR16b(v(rd)?, v(mr)?, v(mr)?))?;
            }
            raw::emit(machine, de64_BSL16b(v(rd)?, v(ar)?, v(br)?))?;
            rp.set_modified();
            Ok(true)
        }
        Op::VecReduceAdd { a } => {
            let ty = ty_of(machine, g(a));
            let lane = ty.lane();
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let an = ap.load_to_reg()?;
            let rd = rp.alloc_reg()?;
            match lane {
                Type::F64 => raw::emit(machine, de64_FADDPd(v(rd)?, v(an)?))?,
                Type::F32 => {
                    let mut tmp = ScratchReg::new(machine);
                    let t = v(tmp.alloc(FP_BANK)?)?;
                    raw::emit(machine, de64_FADDP4s(t, v(an)?, v(an)?))?;
                    raw::emit(machine, de64_FADDPs(v(rd)?, t))?;
                }
                _ => {
                    let mut tmp = ScratchReg::new(machine);
                    let t = v(tmp.alloc(FP_BANK)?)?;
                    let d = raw::zr(rd)?;
                    match lane {
                        Type::I64 => {
                            raw::emit(machine, de64_ADDPd(t, v(an)?))?;
                            raw::emit(machine, de64_FMOVxd(d, t))?;
                        }
                        Type::I32 => {
                            raw::emit(machine, de64_SADDLV4s(t, v(an)?))?;
                            raw::emit(machine, de64_FMOVxd(d, t))?;
                        }
                        Type::I16 => {
                            raw::emit(machine, de64_SADDLV8h(t, v(an)?))?;
                            raw::emit(machine, de64_FMOVws(d, t))?;
                            canon(machine, rd, Type::I32)?;
                        }
                        _ => {
                            raw::emit(machine, de64_SADDLV16b(t, v(an)?))?;
                            raw::emit(machine, de64_FMOVws(d, t))?;
                            canon(machine, rd, Type::I16)?;
                        }
                    }
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::VecMovmask { a } => {
            let ty = ty_of(machine, g(a));
            let lane = ty.lane();
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let an = ap.load_to_reg()?;
            let rd = rp.alloc_reg()?;
            let d = raw::zr(rd)?;
            let mut tmp = ScratchReg::new(machine);
            let t = v(tmp.alloc(FP_BANK)?)?;
            let mut tmp2 = ScratchReg::new(machine);
            let x = tmp2.alloc_gp()?;
            let xz = raw::zr(x)?;
            // The sign bit of every lane to bit 0 of the lane, then the
            // lanes folded into consecutive bits of the result.
            let fold8 = |m: &M, d: GRegZr, dsp: GRegSp| -> PgResult<()> {
                // 8 bytes of 0/1 in `d` -> 8 bits.
                raw::emit(m, de64_ORRx_lsr(d, d, d, 7))?;
                raw::emit(m, de64_ORRx_lsr(d, d, d, 14))?;
                raw::emit(m, de64_ORRx_lsr(d, d, d, 28))?;
                raw::emit(m, de64_ANDxi(dsp, d, 0xff))
            };
            match lane {
                Type::I64 | Type::F64 => {
                    raw::emit(machine, de64_USHR2di(t, v(an)?, 63))?;
                    raw::emit(machine, de64_UMOVxd(d, t, 0))?;
                    raw::emit(machine, de64_UMOVxd(xz, t, 1))?;
                    raw::emit(machine, de64_ORRx_lsl(d, d, xz, 1))?;
                }
                Type::I32 | Type::F32 => {
                    raw::emit(machine, de64_USHR4si(t, v(an)?, 31))?;
                    raw::emit(machine, de64_XTN_4h(t, t))?;
                    raw::emit(machine, de64_XTN_8b(t, t))?;
                    raw::emit(machine, de64_UMOVxd(d, t, 0))?;
                    raw::emit(machine, de64_ORRx_lsr(d, d, d, 7))?;
                    raw::emit(machine, de64_ORRx_lsr(d, d, d, 14))?;
                    raw::emit(machine, de64_ANDxi(raw::sp(rd)?, d, 0xf))?;
                }
                Type::I16 => {
                    raw::emit(machine, de64_USHR8hi(t, v(an)?, 15))?;
                    raw::emit(machine, de64_XTN_8b(t, t))?;
                    raw::emit(machine, de64_UMOVxd(d, t, 0))?;
                    fold8(machine, d, raw::sp(rd)?)?;
                }
                _ => {
                    raw::emit(machine, de64_USHR16bi(t, v(an)?, 7))?;
                    raw::emit(machine, de64_UMOVxd(d, t, 0))?;
                    fold8(machine, d, raw::sp(rd)?)?;
                    raw::emit(machine, de64_UMOVxd(xz, t, 1))?;
                    fold8(machine, xz, raw::sp(x)?)?;
                    raw::emit(machine, de64_ORRx_lsl(d, d, xz, 8))?;
                }
            }
            rp.set_modified();
            Ok(true)
        }
        Op::VecSplat { ty, a } => {
            let lane = ty.lane();
            let f = |m: &M, rd: Reg, rn: Reg| -> PgResult<()> {
                let d = v(rd)?;
                let w = match lane {
                    Type::I64 => de64_DUP2dx(d, raw::zr(rn)?),
                    Type::I32 => de64_DUP4sw(d, raw::zr(rn)?),
                    Type::I16 => de64_DUP8hw(d, raw::zr(rn)?),
                    Type::I8 => de64_DUP16bw(d, raw::zr(rn)?),
                    Type::F64 => de64_DUP2d(d, v(rn)?, 0),
                    Type::F32 => de64_DUP4s(d, v(rn)?, 0),
                    _ => return Err(invalid("gravity_tpde: vsplat lane type")),
                };
                raw::emit(m, w)
            };
            if lane.is_float() {
                unary(machine, g(a), result.unwrap(), f)?;
            } else {
                unary_fresh(machine, g(a), result.unwrap(), f)?;
            }
            Ok(true)
        }
        Op::VecExtract { a, lane } => {
            let lt = ty_of(machine, g(a)).lane();
            let i = lane as u32;
            let f = |m: &M, rd: Reg, rn: Reg| -> PgResult<()> {
                let n = v(rn)?;
                match lt {
                    Type::I64 => raw::emit(m, de64_UMOVxd(raw::zr(rd)?, n, i)),
                    Type::I32 => {
                        raw::emit(m, de64_UMOVws(raw::zr(rd)?, n, i))?;
                        canon(m, rd, Type::I32)
                    }
                    Type::I16 => {
                        raw::emit(m, de64_UMOVwh(raw::zr(rd)?, n, i))?;
                        canon(m, rd, Type::I16)
                    }
                    Type::I8 => {
                        raw::emit(m, de64_UMOVwb(raw::zr(rd)?, n, i))?;
                        canon(m, rd, Type::I8)
                    }
                    Type::F64 => raw::emit(m, de64_DUPd(v(rd)?, n, i)),
                    Type::F32 => raw::emit(m, de64_DUPs(v(rd)?, n, i)),
                    _ => Err(invalid("gravity_tpde: vextract lane type")),
                }
            };
            if lt.is_float() {
                unary(machine, g(a), result.unwrap(), f)?;
            } else {
                unary_fresh(machine, g(a), result.unwrap(), f)?;
            }
            Ok(true)
        }
        Op::VecInsert { a, lane, value } => {
            let lt = ty_of(machine, g(a)).lane();
            let i = lane as u32;
            let a_ref = machine.value_ref(g(a))?;
            let mut ap = a_ref.part(0);
            let x_ref = machine.value_ref(g(value))?;
            let mut xp = x_ref.part(0);
            let r_ref = machine.result_ref(result.unwrap())?;
            let mut rp = r_ref.part(0);
            let an = ap.load_to_reg()?;
            let xr = xp.load_to_reg()?;
            let rd = rp.alloc_try_reuse(&mut ap)?;
            if rd != an {
                raw::emit(machine, de64_ORR16b(v(rd)?, v(an)?, v(an)?))?;
            }
            let w = match lt {
                Type::I64 => de64_INSdx(v(rd)?, i, raw::zr(xr)?),
                Type::I32 => de64_INSsw(v(rd)?, i, raw::zr(xr)?),
                Type::I16 => de64_INShw(v(rd)?, i, raw::zr(xr)?),
                Type::I8 => de64_INSbw(v(rd)?, i, raw::zr(xr)?),
                _ => return Err(invalid("gravity_tpde: vinsert of a float lane is not encoded")),
            };
            raw::emit(machine, w)?;
            rp.set_modified();
            Ok(true)
        }
        Op::VecWiden { a, signed, high } => {
            let lt = ty_of(machine, g(a)).lane();
            unary(machine, g(a), result.unwrap(), |m, rd, rn| {
                let (d, n) = (v(rd)?, v(rn)?);
                let w = match (lt, signed, high) {
                    (Type::I8, false, false) => de64_UXTL_8h(d, n),
                    (Type::I8, false, true) => de64_UXTL2_8h(d, n),
                    (Type::I8, true, false) => de64_SXTL_8h(d, n),
                    (Type::I8, true, true) => de64_SXTL2_8h(d, n),
                    (Type::I16, false, false) => de64_UXTL_4s(d, n),
                    (Type::I16, false, true) => de64_UXTL2_4s(d, n),
                    (Type::I16, true, false) => de64_SXTL_4s(d, n),
                    (Type::I16, true, true) => de64_SXTL2_4s(d, n),
                    (Type::I32, false, false) => de64_UXTL_2d(d, n),
                    (Type::I32, false, true) => de64_UXTL2_2d(d, n),
                    (Type::I32, true, false) => de64_SXTL_2d(d, n),
                    (Type::I32, true, true) => de64_SXTL2_2d(d, n),
                    _ => return Err(invalid("gravity_tpde: vwiden lane type")),
                };
                raw::emit(m, w)
            })?;
            Ok(true)
        }
        Op::VecNarrow { a, b } => {
            let lt = ty_of(machine, g(a)).lane();
            // `xtn2` reads `rd`'s low half: the result must not alias `b`.
            binary_fresh(machine, g(a), g(b), result.unwrap(), |m, rd, rn, rm| {
                let (d, n, r) = (v(rd)?, v(rn)?, v(rm)?);
                let (w1, w2) = match lt {
                    Type::I64 => (de64_XTN_2s(d, n), de64_XTN2_4s(d, r)),
                    Type::I32 => (de64_XTN_4h(d, n), de64_XTN2_8h(d, r)),
                    Type::I16 => (de64_XTN_8b(d, n), de64_XTN2_16b(d, r)),
                    _ => return Err(invalid("gravity_tpde: vnarrow lane type")),
                };
                raw::emit(m, w1)?;
                raw::emit(m, w2)
            })?;
            Ok(true)
        }
        _ => unreachable!("not a vector op"),
    }
}

#[cfg(test)]
mod tests {
    use super::{GravityFrontend, A};
    use tpde_compiler_a64::{A64Frontend, AAPCS_INFO};

    /// `FIXED_ASSIGNMENTS` and `PINNED_ASSIGNMENTS` are the register allocator's budgets per bank (general, vector):
    /// together they fit the bank's allocatable registers and leave scratch ones, which no catalog literal would.
    #[test]
    fn fixed_and_pinned_assignments_are_register_budgets_per_bank() {
        let fixed = <GravityFrontend as A64Frontend<A<'static>>>::FIXED_ASSIGNMENTS;
        let pinned = <GravityFrontend as A64Frontend<A<'static>>>::PINNED_ASSIGNMENTS;
        let regs = AAPCS_INFO.allocatable_regs();
        let banks = [(regs as u32).count_ones(), ((regs >> u32::BITS) as u32).count_ones()];
        for (bank, allocatable) in banks.into_iter().enumerate() {
            assert!(fixed[bank] + pinned[bank] < allocatable, "bank {bank}: {} fixed + {} pinned >= {allocatable}", fixed[bank], pinned[bank]);
        }
    }
}
