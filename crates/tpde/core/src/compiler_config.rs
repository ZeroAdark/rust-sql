// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::calling_convention::CallingConventionAssigner;

mod sealed {
    pub trait Banks {}
    pub trait VariableRefs {}
}

pub struct Banks<const N: usize>;

pub trait RegisterBanks: sealed::Banks {
    type FixedAssignments: AsRef<[u32]> + Copy;
    const COUNT: u32;
}

impl<const N: usize> sealed::Banks for Banks<N> {}

impl<const N: usize> RegisterBanks for Banks<N> {
    type FixedAssignments = [u32; N];
    const COUNT: u32 = {
        assert!(N <= u32::MAX as usize, "register bank count exceeds u32");
        N as u32
    };
}

pub enum DefaultVariableReferences {}
pub enum CustomVariableReferences {}

impl sealed::VariableRefs for DefaultVariableReferences {}
impl sealed::VariableRefs for CustomVariableReferences {}

pub trait VariableReferencePolicy: sealed::VariableRefs {
    const DEFAULT: bool;
}

impl VariableReferencePolicy for DefaultVariableReferences {
    const DEFAULT: bool = true;
}

impl VariableReferencePolicy for CustomVariableReferences {
    const DEFAULT: bool = false;
}

pub trait CompilerConfig {
    const FRAME_INDEXING_NEGATIVE: bool;
    const PLATFORM_POINTER_SIZE: u32;
    const NUM_BANKS: u32;
    const DEFAULT_VAR_REF_HANDLING: bool;

    type DefaultCCAssigner<'cc>: CallingConventionAssigner;
    type Assembler;
    type AsmReg;
    type FunctionWriter;

    type Banks: RegisterBanks;
    type VariableReferences: VariableReferencePolicy;
}

pub struct CompilerConfigDefault;

impl CompilerConfigDefault {
    pub const DEFAULT_VAR_REF_HANDLING: bool = true;
}

pub const fn check_config<C: CompilerConfig>() {
    const {
        assert!(
            C::NUM_BANKS == C::Banks::COUNT,
            "compiler register bank count mismatch"
        );
        assert!(
            C::DEFAULT_VAR_REF_HANDLING == C::VariableReferences::DEFAULT,
            "compiler variable reference policy mismatch"
        );
    }
}
