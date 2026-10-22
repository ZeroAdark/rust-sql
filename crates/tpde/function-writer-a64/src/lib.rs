// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;
mod writer_guard;
pub use writer_guard::{A64Target, A64WriteInst, FunctionWriterA64};

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
