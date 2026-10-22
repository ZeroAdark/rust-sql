// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

use crate::{AsmReg, CCAssignerAAPCS};
use core::marker::PhantomData;
use tpde_core::{
    calling_convention::CCAssigner,
    compiler_config::{Banks, CompilerConfig, DefaultVariableReferences},
};

pub struct PlatformConfig<'m>(PhantomData<&'m ()>);
impl<'m> CompilerConfig for PlatformConfig<'m> {
    const FRAME_INDEXING_NEGATIVE: bool = false;
    const PLATFORM_POINTER_SIZE: u32 = 8;
    const NUM_BANKS: u32 = 2;
    const DEFAULT_VAR_REF_HANDLING: bool = true;
    type DefaultCCAssigner<'cc> = CCAssigner<'cc, CCAssignerAAPCS>;
    type Assembler = tpde_assembler::AssemblerElf<'m>;
    type AsmReg = AsmReg;
    type FunctionWriter = tpde_function_writer_a64::FunctionWriterA64<'m>;
    type Banks = Banks<2>;
    type VariableReferences = DefaultVariableReferences;
}
