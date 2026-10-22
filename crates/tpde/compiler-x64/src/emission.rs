// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
use crate::{emit, invalid, Jump, TLSModel, X64CompilerState, CPU_AVX, CPU_AVX512F};
use tpde_assembler::{elf, SectionKind, SymBinding, SymRef};
use tpde_base::ValLocalIdx;
use tpde_compiler::{
    generic_value_part::ExprReg,
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor, GenericValuePart, ScratchReg, ValuePartRef,
};
use tpde_core::{Reg, RegBank};
use tpde_fadec::*;
use tpde_function_writer::{Label, LabelFixupKind};
use tpde_util::{tpde_assert, tpde_assert_eq};
use types_error::PgResult;

macro_rules! insn {
    ($machine:expr, $instruction:ident $(, $operand:expr)* $(,)?) => {
        emit($machine, |output| $instruction(output, 0 $(, $operand)*))
    };
}

pub fn gp(reg: Reg) -> FeRegGP {
    FE_GP(reg.id()).expect("X64 operand is not a GP register")
}

pub fn xmm(reg: Reg) -> FeRegXMM {
    tpde_assert!(
        (32..64).contains(&reg.id()),
        "X64 operand is not a vector register"
    );
    FE_XMM(reg.id() - 32).expect("X64 vector register exceeds encoder range")
}

fn mem(base: FeRegGP, scale: u8, index: FeRegGP, offset: i32) -> FeMem {
    FE_MEM(base, scale, index, offset).expect("invalid X64 memory operand")
}

fn has_cpu_feats<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    features: u32,
) -> bool
where
    M::Target: X64CompilerState,
{
    machine.with_target(|target| target.x64().cpu_feats & features == features)
}

fn may_clobber_flags<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(machine: &M) -> bool
where
    M::Target: X64CompilerState,
{
    machine.with_target(|target| !target.x64().preserve_flags)
}

pub fn spill_reg<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    reg: Reg,
    frame_off: i32,
    size: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(machine.frame_used());
    tpde_assert!(frame_off < 0);
    let memory = mem(FE_BP, 0, FE_NOREG, frame_off);
    if reg.id() <= 15 {
        let reg = gp(reg);
        match size {
            1 => insn!(machine, fe64_MOV8mr, memory, reg),
            2 => insn!(machine, fe64_MOV16mr, memory, reg),
            4 => insn!(machine, fe64_MOV32mr, memory, reg),
            8 => insn!(machine, fe64_MOV64mr, memory, reg),
            _ => Err(invalid("invalid X64 GP spill size")),
        }
    } else {
        let reg = xmm(reg);
        match size {
            4 => insn!(machine, fe64_SSE_MOVD_X2Gmr, memory, reg),
            8 => insn!(machine, fe64_SSE_MOVQ_X2Gmr, memory, reg),
            16 => insn!(machine, fe64_SSE_MOVAPDmr, memory, reg),
            _ => Err(invalid("invalid X64 vector spill size")),
        }
    }
}

pub fn load_from_stack<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    frame_off: i32,
    size: u32,
    sign_extend: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(machine.frame_used());
    let memory = mem(FE_BP, 0, FE_NOREG, frame_off);
    if dst.id() <= 15 {
        let dst = gp(dst);
        match (size, sign_extend) {
            (1, false) => insn!(machine, fe64_MOVZXr32m8, dst, memory),
            (2, false) => insn!(machine, fe64_MOVZXr32m16, dst, memory),
            (4, false) => insn!(machine, fe64_MOV32rm, dst, memory),
            (1, true) => insn!(machine, fe64_MOVSXr64m8, dst, memory),
            (2, true) => insn!(machine, fe64_MOVSXr64m16, dst, memory),
            (4, true) => insn!(machine, fe64_MOVSXr64m32, dst, memory),
            (8, _) => insn!(machine, fe64_MOV64rm, dst, memory),
            _ => Err(invalid("invalid X64 GP reload size")),
        }
    } else {
        tpde_assert!(!sign_extend);
        let dst = xmm(dst);
        match size {
            4 => insn!(machine, fe64_SSE_MOVD_G2Xrm, dst, memory),
            8 => insn!(machine, fe64_SSE_MOVQ_G2Xrm, dst, memory),
            16 => insn!(machine, fe64_SSE_MOVAPDrm, dst, memory),
            _ => Err(invalid("invalid X64 vector reload size")),
        }
    }
}

pub fn load_address_of_stack_var<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    local: ValLocalIdx,
    part: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(machine.frame_used());
    let offset = machine.with_part(local, part, |part| part.variable_stack_off());
    insn!(
        machine,
        fe64_LEA64rm,
        gp(dst),
        mem(FE_BP, 0, FE_NOREG, offset)
    )
}

pub fn mov<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    size: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(dst.valid() && src.valid());
    if dst.id() <= 15 && src.id() <= 15 {
        if size > 4 {
            insn!(machine, fe64_MOV64rr, gp(dst), gp(src))
        } else {
            insn!(machine, fe64_MOV32rr, gp(dst), gp(src))
        }
    } else if dst.id() >= 32 && src.id() >= 32 {
        if size <= 16 {
            if dst.id() > 47 || src.id() > 47 {
                tpde_assert!(has_cpu_feats(machine, CPU_AVX512F));
                insn!(machine, fe64_VMOVAPD128rr, xmm(dst), xmm(src))
            } else {
                insn!(machine, fe64_SSE_MOVAPDrr, xmm(dst), xmm(src))
            }
        } else if size <= 32 {
            tpde_assert!(has_cpu_feats(machine, CPU_AVX));
            tpde_assert!((dst.id() <= 47 && src.id() <= 47) || has_cpu_feats(machine, CPU_AVX512F));
            insn!(machine, fe64_VMOVAPD256rr, xmm(dst), xmm(src))
        } else {
            tpde_assert!(size <= 64 && has_cpu_feats(machine, CPU_AVX512F));
            insn!(machine, fe64_VMOVAPD512rr, xmm(dst), xmm(src))
        }
    } else if dst.id() <= 15 {
        tpde_assert!(src.id() >= 32 && size <= 8);
        if src.id() > 47 {
            tpde_assert!(has_cpu_feats(machine, CPU_AVX512F));
            if size <= 4 {
                insn!(machine, fe64_VMOVD_X2Grr, gp(dst), xmm(src))
            } else {
                insn!(machine, fe64_VMOVQ_X2Grr, gp(dst), xmm(src))
            }
        } else if size <= 4 {
            insn!(machine, fe64_SSE_MOVD_X2Grr, gp(dst), xmm(src))
        } else {
            insn!(machine, fe64_SSE_MOVQ_X2Grr, gp(dst), xmm(src))
        }
    } else {
        tpde_assert!(src.id() <= 15 && dst.id() >= 32 && size <= 8);
        if dst.id() > 47 {
            tpde_assert!(has_cpu_feats(machine, CPU_AVX512F));
            if size <= 4 {
                insn!(machine, fe64_VMOVD_G2Xrr, xmm(dst), gp(src))
            } else {
                insn!(machine, fe64_VMOVQ_G2Xrr, xmm(dst), gp(src))
            }
        } else if size <= 4 {
            insn!(machine, fe64_SSE_MOVD_G2Xrr, xmm(dst), gp(src))
        } else {
            insn!(machine, fe64_SSE_MOVQ_G2Xrr, xmm(dst), gp(src))
        }
    }
}

pub fn gval_expr_as_reg<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    value: &mut GenericValuePart<'s, M, 2>,
) -> PgResult<Reg>
where
    M::Target: X64CompilerState,
{
    let GenericValuePart::Expr(mut expr) = value.take() else {
        panic!("X64 expression materialization requires an expression");
    };
    let disp32 = expr.disp as i32 as i64 == expr.disp;
    let mut base = if expr.has_base() {
        expr.base_reg()
    } else {
        Reg::make_invalid()
    };
    let index = if expr.has_index() {
        expr.index_reg()
    } else {
        Reg::make_invalid()
    };
    let mut scratch = match &mut expr.base {
        ExprReg::Scratch(scratch) => scratch.take(),
        ExprReg::Borrowed(_) => match &mut expr.index {
            ExprReg::Scratch(scratch) => scratch.take(),
            ExprReg::Borrowed(_) => {
                let mut scratch = ScratchReg::new(machine);
                scratch.alloc_gp()?;
                scratch
            }
        },
    };
    let dst = scratch.cur_reg();
    if index.valid() {
        let scale = expr.scale as u64;
        if scale.is_power_of_two() && scale < 16 {
            let base_reg = if base.valid() { gp(base) } else { FE_NOREG };
            let disp = if disp32 { expr.disp as i32 } else { 0 };
            insn!(
                machine,
                fe64_LEA64rm,
                gp(dst),
                mem(base_reg, scale as u8, gp(index), disp)
            )?;
            if disp32 {
                // A displacement consumed by LEA must not be added a second time.
                expr.disp = 0;
            }
        } else {
            tpde_assert!(may_clobber_flags(machine));
            let mut scale = scale;
            if base == index {
                base = Reg::make_invalid();
                scale = scale.wrapping_add(1);
            }
            let mut index_scratch = ScratchReg::new(machine);
            let mut index_tmp = dst;
            if dst == base {
                index_tmp = match &expr.index {
                    ExprReg::Scratch(scratch) if scratch.has_reg() => scratch.cur_reg(),
                    _ => index_scratch.alloc_gp()?,
                };
            }
            if scale.is_power_of_two() {
                if index_tmp != index {
                    insn!(machine, fe64_MOV64rr, gp(index_tmp), gp(index))?;
                }
                insn!(
                    machine,
                    fe64_SHL64ri,
                    gp(index_tmp),
                    scale.trailing_zeros() as _
                )?;
            } else if scale as i32 as i64 == scale as i64 {
                insn!(
                    machine,
                    fe64_IMUL64rri,
                    gp(index_tmp),
                    gp(index),
                    scale as _
                )?;
            } else {
                let mut scratch2 = ScratchReg::new(machine);
                let tmp2 = scratch2.alloc_gp()?;
                insn!(machine, fe64_MOV64ri, gp(tmp2), scale as _)?;
                if index_tmp != index {
                    insn!(machine, fe64_MOV64rr, gp(index_tmp), gp(index))?;
                }
                insn!(machine, fe64_IMUL64rr, gp(index_tmp), gp(tmp2))?;
            }
            if base.valid() {
                if disp32 {
                    insn!(
                        machine,
                        fe64_LEA64rm,
                        gp(dst),
                        mem(gp(base), 1, gp(index_tmp), expr.disp as i32)
                    )?;
                    expr.disp = 0;
                } else if dst == base {
                    insn!(machine, fe64_ADD64rr, gp(dst), gp(index_tmp))?;
                } else {
                    insn!(machine, fe64_ADD64rr, gp(dst), gp(base))?;
                }
            }
        }
    } else if base.valid() {
        if expr.disp != 0 && disp32 {
            insn!(
                machine,
                fe64_LEA64rm,
                gp(dst),
                mem(gp(base), 0, FE_NOREG, expr.disp as i32)
            )?;
            expr.disp = 0;
        } else if dst != base {
            insn!(machine, fe64_MOV64rr, gp(dst), gp(base))?;
        }
    } else {
        insn!(machine, fe64_MOV64ri, gp(dst), expr.disp)?;
        expr.disp = 0;
    }
    if expr.disp != 0 {
        let mut scratch2 = ScratchReg::new(machine);
        let tmp2 = scratch2.alloc_gp()?;
        insn!(machine, fe64_MOV64ri, gp(tmp2), expr.disp)?;
        if may_clobber_flags(machine) {
            insn!(machine, fe64_ADD64rr, gp(dst), gp(tmp2))?;
        } else {
            insn!(machine, fe64_LEA64rm, gp(dst), mem(gp(dst), 1, gp(tmp2), 0))?;
        }
    }
    *value = GenericValuePart::Scratch(scratch.take());
    Ok(dst)
}

pub fn alloca_fixed<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    size: u64,
    align: u32,
    result: &mut ValuePartRef<'_, M, 2>,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(machine.frame_has_dynamic_alloca());
    tpde_assert!(align.is_power_of_two());
    tpde_assert!(may_clobber_flags(machine));
    let size = size
        .checked_add(15)
        .ok_or_else(|| invalid("X64 alloca size overflow"))?
        & !15;
    if size >= 0x8000_0000 || align >= 1 << 31 {
        return Err(invalid("X64 alloca exceeds immediate range"));
    }
    if size != 0 {
        insn!(machine, fe64_SUB64ri, FE_SP, size as _)?;
    }
    if align > 16 {
        insn!(machine, fe64_AND64ri, FE_SP, (!(align - 1)) as _)?;
    }
    let reg = result.alloc_reg()?;
    insn!(machine, fe64_MOV64rr, gp(reg), FE_SP)
}

pub fn alloca_dynamic<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    elem_size: u64,
    mut count: ValuePartRef<'s, M, 2>,
    align: u32,
    result: &mut ValuePartRef<'s, M, 2>,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(machine.frame_has_dynamic_alloca());
    tpde_assert!(align.is_power_of_two());
    tpde_assert!(may_clobber_flags(machine));
    let align = align.max(16);
    if elem_size & u64::from(align - 1) != 0 && align >= 1 << 31 {
        return Err(invalid("X64 alloca alignment exceeds immediate range"));
    }
    let size_reg = count.cur_reg_or_load()?;
    let result_reg = result.alloc_try_reuse(&mut count)?;
    if elem_size == 0 {
        insn!(machine, fe64_XOR32rr, gp(result_reg), gp(result_reg))?;
    } else if elem_size.is_power_of_two() {
        let shift = elem_size.trailing_zeros();
        if shift > 0 && shift < 4 {
            insn!(
                machine,
                fe64_LEA64rm,
                gp(result_reg),
                mem(FE_NOREG, (1 << shift) as u8, gp(size_reg), 0)
            )?;
        } else {
            if size_reg != result_reg {
                insn!(machine, fe64_MOV64rr, gp(result_reg), gp(size_reg))?;
            }
            if elem_size != 1 {
                insn!(machine, fe64_SHL64ri, gp(result_reg), shift as _)?;
            }
        }
    } else if elem_size <= 0x7fff_ffff {
        insn!(
            machine,
            fe64_IMUL64rri,
            gp(result_reg),
            gp(size_reg),
            elem_size as _
        )?;
    } else {
        let mut scratch = ScratchReg::new(machine);
        let tmp = scratch.alloc_gp()?;
        insn!(machine, fe64_MOV64ri, gp(tmp), elem_size as _)?;
        if size_reg != result_reg {
            insn!(machine, fe64_MOV64rr, gp(result_reg), gp(size_reg))?;
        }
        insn!(machine, fe64_IMUL64rr, gp(result_reg), gp(tmp))?;
    }
    insn!(machine, fe64_SUB64rr, FE_SP, gp(result_reg))?;
    if elem_size & u64::from(align - 1) != 0 {
        insn!(machine, fe64_AND64ri, FE_SP, (!(align - 1)) as _)?;
    }
    insn!(machine, fe64_MOV64rr, gp(result_reg), FE_SP)
}

pub fn materialize_constant<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    data: &[u64],
    bank: RegBank,
    size: u32,
    dst: Reg,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if data.is_empty() || data.len() < (u64::from(size) + 7) as usize / 8 {
        return Err(invalid(
            "X64 constant data is shorter than its declared size",
        ));
    }
    let low = data[0];
    if bank == RegBank::new(0) {
        tpde_assert!(size <= 8);
        if low == 0 {
            if may_clobber_flags(machine) {
                return insn!(machine, fe64_XOR32rr, gp(dst), gp(dst));
            }
            return insn!(machine, fe64_MOV32ri, gp(dst), 0);
        }
        if size <= 4 || low <= u32::MAX as u64 {
            return insn!(machine, fe64_MOV32ri, gp(dst), low as _);
        }
        return insn!(machine, fe64_MOV64ri, gp(dst), low as _);
    }
    tpde_assert_eq!(bank, RegBank::new(1));
    if size > 16 {
        return Err(invalid(
            "X64 cannot materialize vector constants wider than 128 bits",
        ));
    }
    let high = if size <= 8 { 0 } else { data[1] };
    let avx = has_cpu_feats(machine, CPU_AVX);
    if low == 0 && high == 0 {
        return if avx {
            insn!(machine, fe64_VPXOR128rrr, xmm(dst), xmm(dst), xmm(dst))
        } else {
            insn!(machine, fe64_SSE_PXORrr, xmm(dst), xmm(dst))
        };
    }
    if low == u64::MAX && (size <= 8 || high == u64::MAX) {
        return if avx {
            insn!(machine, fe64_VPCMPEQB128rrr, xmm(dst), xmm(dst), xmm(dst))
        } else {
            insn!(machine, fe64_SSE_PCMPEQBrr, xmm(dst), xmm(dst))
        };
    }
    if size <= 8 {
        let tmp = machine.with_registers(|registers| {
            let tmp = registers.find_first_free_excluding(RegBank::new(0), 0);
            if tmp.valid() {
                registers.mark_clobbered(tmp);
            }
            tmp
        });
        if tmp.valid() {
            materialize_constant(machine, data, RegBank::new(0), size, tmp)?;
            return match (size <= 4, avx) {
                (true, true) => insn!(machine, fe64_VMOVD_G2Xrr, xmm(dst), gp(tmp)),
                (true, false) => insn!(machine, fe64_SSE_MOVD_G2Xrr, xmm(dst), gp(tmp)),
                (false, true) => insn!(machine, fe64_VMOVQ_G2Xrr, xmm(dst), gp(tmp)),
                (false, false) => insn!(machine, fe64_SSE_MOVQ_G2Xrr, xmm(dst), gp(tmp)),
            };
        }
    }
    let allocation_size = (size + 7) & !7;
    let mut bytes = [0u8; 16];
    for (word, output) in data
        .iter()
        .zip(bytes.chunks_exact_mut(8))
        .take(allocation_size as usize / 8)
    {
        output.copy_from_slice(&word.to_le_bytes());
    }
    let symbol = machine.with_output(|output| {
        let assembler = output.assembler_mut();
        let section = assembler.get_default_section(SectionKind::ReadOnly)?;
        assembler
            .sym_def_data(
                section,
                b"",
                &bytes[..allocation_size as usize],
                allocation_size,
                SymBinding::Local,
            )
            .map(|(symbol, _)| symbol)
    })?;
    let memory = mem(FE_IP, 0, FE_NOREG, -1);
    if size <= 4 {
        if avx {
            insn!(machine, fe64_VMOVSSrm, xmm(dst), memory)?;
        } else {
            insn!(machine, fe64_SSE_MOVSSrm, xmm(dst), memory)?;
        }
    } else if size <= 8 {
        if avx {
            insn!(machine, fe64_VMOVSDrm, xmm(dst), memory)?;
        } else {
            insn!(machine, fe64_SSE_MOVSDrm, xmm(dst), memory)?;
        }
    } else if avx {
        insn!(machine, fe64_VMOVAPS128rm, xmm(dst), memory)?;
    } else {
        insn!(machine, fe64_SSE_MOVAPSrm, xmm(dst), memory)?;
    }
    let offset = machine.with_output(|output| output.offset());
    machine.reloc_text(symbol, elf::R_X86_64_PC32, (offset - 4) as u64, -4)
}

pub const fn invert_jump(jump: Jump) -> Jump {
    match jump {
        Jump::ja => Jump::jbe,
        Jump::jae => Jump::jb,
        Jump::jb => Jump::jae,
        Jump::jbe => Jump::ja,
        Jump::je => Jump::jne,
        Jump::jg => Jump::jle,
        Jump::jge => Jump::jl,
        Jump::jl => Jump::jge,
        Jump::jle => Jump::jg,
        Jump::jne => Jump::je,
        Jump::jno => Jump::jo,
        Jump::jo => Jump::jno,
        Jump::js => Jump::jns,
        Jump::jns => Jump::js,
        Jump::jp => Jump::jnp,
        Jump::jnp => Jump::jp,
        Jump::jmp => panic!("unconditional jump cannot be inverted"),
    }
}

pub const fn swap_jump(jump: Jump) -> Jump {
    match jump {
        Jump::ja => Jump::jb,
        Jump::jae => Jump::jbe,
        Jump::jb => Jump::ja,
        Jump::jbe => Jump::jae,
        Jump::je => Jump::je,
        Jump::jne => Jump::jne,
        Jump::jg => Jump::jl,
        Jump::jge => Jump::jle,
        Jump::jl => Jump::jg,
        Jump::jle => Jump::jge,
        _ => panic!("jump does not represent an order comparison"),
    }
}

pub const fn jump_to_cond(jump: Jump) -> FeCond {
    assert!((jump as u32) < 16, "unconditional jump has no condition");
    (jump as u32) << 16
}

pub fn generate_raw_jump<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    jump: Jump,
    label: Label,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let (pending, target) = machine.with_output(|output| {
        output.with_base(|base| {
            let pending = base.label_is_pending(label)?;
            let target = if pending {
                base.offset()
            } else {
                base.label_offset(label)? as usize
            };
            Ok::<_, alloc::boxed::Box<types_error::PgError>>((pending, target))
        })
    })?;
    let flags = if pending { FE_JMPL } else { 0 };
    if jump == Jump::jmp {
        emit(machine, |output| fe64_JMP(output, flags, target))?;
    } else {
        emit(machine, |output| {
            fe64_Jcc(output, flags | jump_to_cond(jump), target)
        })?;
    }
    if pending {
        machine.with_output(|output| {
            output.with_base(|base| {
                let offset = u32::try_from(base.offset() - 4)
                    .map_err(|_| invalid("X64 jump fixup offset exceeds u32"))?;
                base.label_ref(label, offset, LabelFixupKind::X64_JMP_OR_MEM_DISP)
            })
        })?;
    }
    Ok(())
}

pub fn generate_raw_set<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    cc: Jump,
    dst: Reg,
    zero_extend: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let flags = jump_to_cond(cc);
    let dst = gp(dst);
    if zero_extend {
        insn!(machine, fe64_MOV32ri, dst, 0)?;
    }
    emit(machine, |output| fe64_SETcc8r(output, flags, dst))
}

pub fn generate_raw_mask<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    cc: Jump,
    dst: Reg,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    generate_raw_set(machine, cc, dst, true)?;
    insn!(machine, fe64_NEG64r, gp(dst))
}

pub fn generate_raw_cmov<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    cc: Jump,
    dst: Reg,
    src: Reg,
    is_64: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    let flags = jump_to_cond(cc);
    if is_64 {
        emit(machine, |output| {
            fe64_CMOVcc64rr(output, flags, gp(dst), gp(src))
        })
    } else {
        emit(machine, |output| {
            fe64_CMOVcc32rr(output, flags, gp(dst), gp(src))
        })
    }
}

pub fn generate_raw_intext<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    sign: bool,
    from: u32,
    to: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(from < to && to <= 64);
    tpde_assert!(may_clobber_flags(machine));
    if !sign {
        match from {
            8 => insn!(machine, fe64_MOVZXr32r8, gp(dst), gp(src))?,
            16 => insn!(machine, fe64_MOVZXr32r16, gp(dst), gp(src))?,
            32 => insn!(machine, fe64_MOV32rr, gp(dst), gp(src))?,
            _ if from < 32 => {
                if dst != src {
                    insn!(machine, fe64_MOV32rr, gp(dst), gp(src))?;
                }
                insn!(machine, fe64_AND32ri, gp(dst), ((1u32 << from) - 1) as _)?;
            }
            _ if dst != src => {
                insn!(machine, fe64_MOV64ri, gp(dst), ((1u64 << from) - 1) as _)?;
                insn!(machine, fe64_AND64rr, gp(dst), gp(src))?;
            }
            _ => {
                let mut scratch = ScratchReg::new(machine);
                let tmp = scratch.alloc_gp()?;
                insn!(machine, fe64_MOV64ri, gp(tmp), ((1u64 << from) - 1) as _)?;
                insn!(machine, fe64_AND64rr, gp(dst), gp(tmp))?;
            }
        }
    } else if to <= 32 {
        match from {
            8 => insn!(machine, fe64_MOVSXr32r8, gp(dst), gp(src))?,
            16 => insn!(machine, fe64_MOVSXr32r16, gp(dst), gp(src))?,
            _ => {
                if dst != src {
                    insn!(machine, fe64_MOV32rr, gp(dst), gp(src))?;
                }
                if from == 1 {
                    insn!(machine, fe64_AND32ri, gp(dst), 1)?;
                    insn!(machine, fe64_NEG32r, gp(dst))?;
                } else {
                    insn!(machine, fe64_SHL32ri, gp(dst), (32 - from) as _)?;
                    insn!(machine, fe64_SAR32ri, gp(dst), (32 - from) as _)?;
                }
            }
        }
    } else {
        match from {
            8 => insn!(machine, fe64_MOVSXr64r8, gp(dst), gp(src))?,
            16 => insn!(machine, fe64_MOVSXr64r16, gp(dst), gp(src))?,
            32 => insn!(machine, fe64_MOVSXr64r32, gp(dst), gp(src))?,
            _ => {
                if dst != src {
                    if from < 32 {
                        insn!(machine, fe64_MOV32rr, gp(dst), gp(src))?;
                    } else {
                        insn!(machine, fe64_MOV64rr, gp(dst), gp(src))?;
                    }
                }
                if from == 1 {
                    insn!(machine, fe64_AND32ri, gp(dst), 1)?;
                    insn!(machine, fe64_NEG64r, gp(dst))?;
                } else {
                    insn!(machine, fe64_SHL64ri, gp(dst), (64 - from) as _)?;
                    insn!(machine, fe64_SAR64ri, gp(dst), (64 - from) as _)?;
                }
            }
        }
    }
    Ok(())
}

pub fn generate_raw_bfi<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    lsb: u32,
    width: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(lsb < 63 && width < 64 && lsb + width <= 64 && width != 0);
    tpde_assert!(may_clobber_flags(machine));
    let mut scratch = ScratchReg::new(machine);
    let tmp = scratch.alloc_gp()?;
    if dst == src {
        // Preserve the source before clearing an overlapping destination field.
        insn!(machine, fe64_MOV64rr, gp(tmp), gp(src))?;
        return generate_raw_bfi(machine, dst, tmp, lsb, width);
    }
    if width == 1 {
        insn!(machine, fe64_BTR64ri, gp(dst), lsb as _)?;
    } else if lsb + width <= 31 {
        insn!(
            machine,
            fe64_AND64ri,
            gp(dst),
            (!(((1u64 << width) - 1) << lsb)) as _
        )?;
    } else {
        insn!(
            machine,
            fe64_MOV64ri,
            gp(tmp),
            (!(((1u64 << width) - 1) << lsb)) as _
        )?;
        insn!(machine, fe64_AND64rr, gp(dst), gp(tmp))?;
    }
    if width == 8 {
        insn!(machine, fe64_MOVZXr32r8, gp(tmp), gp(src))?;
    } else if width == 16 {
        insn!(machine, fe64_MOVZXr32r16, gp(tmp), gp(src))?;
    } else if width <= 32 {
        insn!(machine, fe64_MOV32rr, gp(tmp), gp(src))?;
        if width < 32 {
            insn!(machine, fe64_AND32ri, gp(tmp), ((1u32 << width) - 1) as _)?;
        }
    } else {
        insn!(machine, fe64_MOV64ri, gp(tmp), ((1u64 << width) - 1) as _)?;
        insn!(machine, fe64_AND64rr, gp(tmp), gp(src))?;
    }
    if (1..=3).contains(&lsb) {
        insn!(
            machine,
            fe64_LEA64rm,
            gp(dst),
            mem(gp(dst), 1 << lsb, gp(tmp), 0)
        )?;
    } else {
        if lsb > 0 {
            if lsb + width <= 32 {
                insn!(machine, fe64_SHL32ri, gp(tmp), lsb as _)?;
            } else {
                insn!(machine, fe64_SHL64ri, gp(tmp), lsb as _)?;
            }
        }
        insn!(machine, fe64_OR64rr, gp(dst), gp(tmp))?;
    }
    Ok(())
}

pub fn generate_raw_bfiz<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    dst: Reg,
    src: Reg,
    lsb: u32,
    width: u32,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(lsb < 63 && width < 64 && lsb + width <= 64 && width != 0);
    tpde_assert!(dst != src);
    tpde_assert!(may_clobber_flags(machine));
    if width == 8 {
        insn!(machine, fe64_MOVZXr32r8, gp(dst), gp(src))?;
    } else if width == 16 {
        insn!(machine, fe64_MOVZXr32r16, gp(dst), gp(src))?;
    } else if width <= 32 {
        insn!(machine, fe64_MOV32rr, gp(dst), gp(src))?;
        if width < 32 {
            insn!(machine, fe64_AND32ri, gp(dst), ((1u32 << width) - 1) as _)?;
        }
    } else {
        insn!(machine, fe64_MOV64ri, gp(dst), ((1u64 << width) - 1) as _)?;
        insn!(machine, fe64_AND64rr, gp(dst), gp(src))?;
    }
    if lsb > 0 {
        if lsb + width <= 32 {
            insn!(machine, fe64_SHL32ri, gp(dst), lsb as _)?;
        } else {
            insn!(machine, fe64_SHL64ri, gp(dst), lsb as _)?;
        }
    }
    Ok(())
}

pub fn switch_emit_cmp<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    if width32 {
        insn!(machine, fe64_CMP32ri, gp(cmp), value as _)
    } else if value as i32 as i64 == value as i64 {
        insn!(machine, fe64_CMP64ri, gp(cmp), value as _)
    } else {
        materialize_constant(machine, &[value], RegBank::new(0), 8, tmp)?;
        insn!(machine, fe64_CMP64rr, gp(cmp), gp(tmp))
    }
}

pub fn switch_emit_cmpeq<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    label: Label,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    switch_emit_cmp(machine, cmp, tmp, value, width32)?;
    generate_raw_jump(machine, Jump::je, label)
}

pub fn switch_create_jump_table<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    default: Label,
    cmp: Reg,
    tmp: Reg,
    low: u64,
    high: u64,
    width32: bool,
    fill: impl FnOnce(&mut [Label]),
) -> PgResult<bool>
where
    M::Target: X64CompilerState,
{
    let range = high
        .checked_sub(low)
        .and_then(|range| range.checked_add(1))
        .and_then(|range| u32::try_from(range).ok())
        .ok_or_else(|| invalid("X64 jump table range exceeds u32"))?;
    let mut needs_extension = width32;
    if low > 0 {
        if width32 {
            needs_extension = false;
            insn!(machine, fe64_SUB32ri, gp(cmp), low as _)?;
        } else if low as i32 as i64 == low as i64 {
            insn!(machine, fe64_SUB64ri, gp(cmp), low as _)?;
        } else {
            materialize_constant(machine, &[low], RegBank::new(0), 8, tmp)?;
            insn!(machine, fe64_SUB64rr, gp(cmp), gp(tmp))?;
        }
    }
    switch_emit_cmp(machine, cmp, tmp, high - low, width32)?;
    generate_raw_jump(machine, Jump::ja, default)?;
    if needs_extension {
        insn!(machine, fe64_MOV32rr, gp(cmp), gp(cmp))?;
    }
    machine.with_output(|output| output.jump_table(range, cmp, tmp, false, fill))?;
    Ok(true)
}

pub fn switch_emit_binary_step<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    equal: Label,
    greater: Label,
    cmp: Reg,
    tmp: Reg,
    value: u64,
    width32: bool,
) -> PgResult<()>
where
    M::Target: X64CompilerState,
{
    switch_emit_cmpeq(machine, equal, cmp, tmp, value, width32)?;
    generate_raw_jump(machine, Jump::ja, greater)
}

pub fn tls_get_addr<'s, 'm, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &'s M,
    symbol: SymRef,
    _model: TLSModel,
) -> PgResult<ScratchReg<'s, M, 2>>
where
    M::Target: X64CompilerState,
{
    tpde_assert!(!machine.call_is_leaf_function());
    tpde_assert!(may_clobber_flags(machine));
    machine.call_set_generated_call();
    let callee_saved = (1u64 << 3) | (0xfu64 << 12);
    let mut used = machine.with_registers(|registers| registers.used.get() & !callee_saved);
    while used != 0 {
        let reg = used.trailing_zeros();
        used &= used - 1;
        machine.evict_reg(Reg::new(u64::from(reg)))?;
    }
    let mut arg = ScratchReg::new(machine);
    let arg_reg = arg.alloc_specific(Reg::new(7))?;
    machine.with_output(|output| output.write_bytes(&[0x66]))?;
    insn!(
        machine,
        fe64_LEA64rm,
        gp(arg_reg),
        mem(FE_IP, 0, FE_NOREG, 0)
    )?;
    let offset = machine.with_output(|output| output.offset());
    machine.reloc_text(symbol, elf::R_X86_64_TLSGD, (offset - 4) as u64, -4)?;
    machine.with_output(|output| output.write_bytes(&[0x66, 0x66, 0x48]))?;
    emit(machine, |output| {
        let target = output.address() + 5;
        fe64_CALL(output, 0, target)
    })?;
    let mut helper = machine.with_target(|target| target.x64().sym_tls_get_addr);
    if !helper.valid() {
        helper = machine.with_output(|output| {
            output
                .assembler_mut()
                .sym_add_undef(b"__tls_get_addr", SymBinding::Global)
        })?;
        machine.with_target(|target| target.x64_mut().sym_tls_get_addr = helper);
    }
    let offset = machine.with_output(|output| output.offset());
    machine.reloc_text(helper, elf::R_X86_64_PLT32, (offset - 4) as u64, -4)?;
    arg.reset();
    let mut result = ScratchReg::new(machine);
    result.alloc_specific(Reg::new(0))?;
    Ok(result)
}
