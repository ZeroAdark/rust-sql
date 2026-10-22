// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
pub mod analyzer;
mod assignment_guard;
pub mod assignment_part_ref;
pub mod calling_convention;
pub mod compiler;
pub mod compiler_config;
pub mod ir_adaptor;
pub mod register_file;
pub mod value_assignment;
pub use analyzer::{Analyzer, BlockIndex, LivenessInfo, Loop, INVALID_BLOCK_IDX};
pub use assignment_part_ref::AssignmentPartRef;
pub use register_file::{Assignment, Reg, RegBank, RegBitSet, RegisterFile};

#[cfg(test)]
extern crate std;
