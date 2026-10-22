// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;

#[cfg(test)]
extern crate self as tpde_compiler_x64;
#[cfg(test)]
extern crate std;
#[cfg(test)]
#[path = "../tests/unit.rs"]
mod tests;

pub mod calling_convention;
pub mod calls;
pub mod emission;
pub mod frame;
pub mod target;

pub use calling_convention::{CCAssignerSysV, CC_INFO};
use mcx::Mcx;
pub use target::X64Frontend;
use tpde_assembler::SymRef;
pub use tpde_compiler::compiler_base::TLSModel;
use tpde_compiler::{
    target::{CompilerOutput, TargetMachine},
    CompilerAdaptor,
};
use tpde_core::Reg;
use tpde_fadec::{EncodeError, Output};
use tpde_util::tpde_assert;
use tpde_util::SmallVectorGuard;
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
pub type AsmReg = Reg;

pub const CPU_BASELINE: u32 = 0;
pub const CPU_CMPXCHG16B: u32 = 1 << 0;
pub const CPU_POPCNT: u32 = 1 << 1;
pub const CPU_SSE3: u32 = 1 << 2;
pub const CPU_SSSE3: u32 = 1 << 3;
pub const CPU_SSE4_1: u32 = 1 << 4;
pub const CPU_SSE4_2: u32 = 1 << 5;
pub const CPU_AVX: u32 = 1 << 6;
pub const CPU_AVX2: u32 = 1 << 7;
pub const CPU_BMI1: u32 = 1 << 8;
pub const CPU_BMI2: u32 = 1 << 9;
pub const CPU_F16C: u32 = 1 << 10;
pub const CPU_FMA: u32 = 1 << 11;
pub const CPU_LZCNT: u32 = 1 << 12;
pub const CPU_MOVBE: u32 = 1 << 13;
pub const CPU_AVX512F: u32 = 1 << 14;
pub const CPU_AVX512BW: u32 = 1 << 15;
pub const CPU_AVX512CD: u32 = 1 << 16;
pub const CPU_AVX512DQ: u32 = 1 << 17;
pub const CPU_AVX512VL: u32 = 1 << 18;
pub const CPU_V2: u32 = (1 << 6) - 1;
pub const CPU_V3: u32 = (1 << 14) - 1;
pub const CPU_V4: u32 = (1 << 19) - 1;
pub const NUM_FIXED_ASSIGNMENTS: [u32; 2] = [5, 6];
pub const MAX_STATIC_ALLOCA_SIZE: u32 = 0x10000000;

#[allow(non_camel_case_types)]
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    jo,
    jno,
    jb,
    jae,
    je,
    jne,
    jbe,
    ja,
    js,
    jns,
    jp,
    jnp,
    jl,
    jge,
    jle,
    jg,
    jmp,
}

#[derive(Clone, Copy, Debug)]
pub struct X64State {
    pub cpu_feats: u32,
    pub fixed_assignment_nonallocatable_mask: u64,
    pub callee_saved_regs: u64,
    pub func_start_off: u32,
    pub func_prologue_alloc: u32,
    pub scalar_arg_count: u32,
    pub vec_arg_count: u32,
    pub reg_save_frame_off: u32,
    pub var_arg_stack_off: u32,
    pub max_callee_stack_arg_size: u32,
    pub preserve_flags: bool,
    pub sym_tls_get_addr: SymRef,
}

impl X64State {
    pub fn new(cpu_feats: u32) -> Self {
        Self {
            cpu_feats,
            fixed_assignment_nonallocatable_mask: 7,
            callee_saved_regs: CC_INFO.callee_saved_regs(),
            func_start_off: 0,
            func_prologue_alloc: 0,
            scalar_arg_count: u32::MAX,
            vec_arg_count: u32::MAX,
            reg_save_frame_off: 0,
            var_arg_stack_off: 0,
            max_callee_stack_arg_size: 0,
            preserve_flags: false,
            sym_tls_get_addr: SymRef::default(),
        }
    }
    pub const fn has_cpu_feats(&self, features: u32) -> bool {
        self.cpu_feats & features == features
    }
    pub fn set_preserve_flags(&mut self, value: bool) {
        self.preserve_flags = value;
    }
    pub const fn may_clobber_flags(&self) -> bool {
        !self.preserve_flags
    }
}

pub trait X64CompilerState {
    type Frontend;
    fn x64(&self) -> &X64State;
    fn x64_mut(&mut self) -> &mut X64State;
    fn frontend(&self) -> &Self::Frontend;
    fn frontend_mut(&mut self) -> &mut Self::Frontend;
    fn return_offsets(&self) -> &[u32];
    fn push_return(&mut self, offset: u32) -> PgResult<()>;
    fn pop_return(&mut self) -> Option<u32>;
    fn clear_returns(&mut self);
}

pub struct CompilerX64<'m, F> {
    state: X64State,
    frontend: F,
    returns: SmallVectorGuard<'m, u32, 8>,
}

impl<'m, F> CompilerX64<'m, F> {
    pub fn new(mcx: Mcx<'m>, frontend: F, cpu_feats: u32) -> Self {
        Self {
            state: X64State::new(cpu_feats),
            frontend,
            returns: SmallVectorGuard::new(mcx),
        }
    }
}

impl<F> X64CompilerState for CompilerX64<'_, F> {
    type Frontend = F;
    fn x64(&self) -> &X64State {
        &self.state
    }
    fn x64_mut(&mut self) -> &mut X64State {
        &mut self.state
    }
    fn frontend(&self) -> &F {
        &self.frontend
    }
    fn frontend_mut(&mut self) -> &mut F {
        &mut self.frontend
    }
    fn return_offsets(&self) -> &[u32] {
        &self.returns
    }
    fn push_return(&mut self, offset: u32) -> PgResult<()> {
        self.returns.push(offset)
    }
    fn pop_return(&mut self) -> Option<u32> {
        self.returns.pop()
    }
    fn clear_returns(&mut self) {
        self.returns.clear();
    }
}

#[cold]
pub fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}

#[cold]
pub fn encoding_error(_: EncodeError) -> alloc::boxed::Box<PgError> {
    invalid("invalid X64 compiler instruction encoding")
}

pub fn emit<'m, A: CompilerAdaptor, M: TargetMachine<'m, A, 2>>(
    machine: &M,
    operation: impl FnOnce(&mut Output<'_>) -> Result<usize, EncodeError>,
) -> PgResult<()> {
    machine.with_output(|writer| {
        let mut bytes = [0; 16];
        let mut output = Output::new(&mut bytes, writer.offset());
        operation(&mut output).map_err(encoding_error)?;
        writer.write_bytes(output.bytes())
    })
}

pub const fn create_bitmask(registers: &[Reg]) -> u64 {
    let mut bits = 0;
    let mut index = 0;
    while index < registers.len() {
        tpde_assert!(registers[index].id() < 64);
        bits |= 1u64 << registers[index].id();
        index += 1;
    }
    bits
}

pub fn val_spill_slot<'c, M: tpde_compiler::guard_machine::GuardMachine<2>>(
    machine: &'c M,
    local: tpde_base::ValLocalIdx,
    part: u32,
) -> tpde_compiler::GenericValuePart<'c, M, 2> {
    let offset = machine.with_part(local, part, |assignment| {
        tpde_assert!(assignment.stack_valid() && !assignment.variable_ref());
        assignment.frame_off()
    });
    tpde_compiler::generic_value_part::Expr::from_reg(regs::BP, i64::from(offset)).into()
}

pub mod regs {
    use tpde_core::Reg;
    pub const AX: Reg = Reg::new(0);
    pub const CX: Reg = Reg::new(1);
    pub const DX: Reg = Reg::new(2);
    pub const BX: Reg = Reg::new(3);
    pub const SP: Reg = Reg::new(4);
    pub const BP: Reg = Reg::new(5);
    pub const SI: Reg = Reg::new(6);
    pub const DI: Reg = Reg::new(7);
    pub const R8: Reg = Reg::new(8);
    pub const R9: Reg = Reg::new(9);
    pub const R10: Reg = Reg::new(10);
    pub const R11: Reg = Reg::new(11);
    pub const R12: Reg = Reg::new(12);
    pub const R13: Reg = Reg::new(13);
    pub const R14: Reg = Reg::new(14);
    pub const R15: Reg = Reg::new(15);
    pub const XMM0: Reg = Reg::new(32);
    pub const XMM1: Reg = Reg::new(33);
    pub const XMM2: Reg = Reg::new(34);
    pub const XMM3: Reg = Reg::new(35);
    pub const XMM4: Reg = Reg::new(36);
    pub const XMM5: Reg = Reg::new(37);
    pub const XMM6: Reg = Reg::new(38);
    pub const XMM7: Reg = Reg::new(39);
    pub const XMM8: Reg = Reg::new(40);
    pub const XMM9: Reg = Reg::new(41);
    pub const XMM10: Reg = Reg::new(42);
    pub const XMM11: Reg = Reg::new(43);
    pub const XMM12: Reg = Reg::new(44);
    pub const XMM13: Reg = Reg::new(45);
    pub const XMM14: Reg = Reg::new(46);
    pub const XMM15: Reg = Reg::new(47);
}

const _: () = {
    assert!(core::mem::size_of::<X64State>() <= 128);
    assert!(!core::mem::needs_drop::<X64State>());
};
