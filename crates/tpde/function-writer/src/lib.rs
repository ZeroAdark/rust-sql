// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;
mod writer_guard;
pub use writer_guard::{
    EhWriterGuard, FunctionWriter, FunctionWriterBase, FunctionWriterJumpTables,
    FunctionWriterTarget, JumpTable, JumpTableHeader, Label, LabelFixup, LabelFixupKind,
    ModuleWriter, TargetCieInfo, TextReservationGuard,
};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
