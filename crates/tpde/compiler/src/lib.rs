// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
extern crate alloc;
pub mod generic_value_part;
pub mod guard_machine;
pub mod scratch_reg;
pub mod value_part_ref;
pub mod value_ref;
pub use generic_value_part::GenericValuePart;
pub use guard_machine::RegallocEvent;
pub use scratch_reg::ScratchReg;
pub use value_part_ref::{ValuePart, ValuePartRef};
pub use value_ref::ValueRef;

#[cfg(test)]
extern crate self as tpde_compiler;
#[cfg(test)]
extern crate std;
pub mod assignment_store_guard;
pub mod branches;
pub mod calls;
mod phi_guard;

#[cfg(test)]
#[path = "../tests/value_guards.rs"]
mod value_guard_tests;

#[cfg(test)]
#[path = "../tests/fixed_register_backup.rs"]
mod fixed_register_backup_tests;

pub mod compiler_base;
mod compiler_guard;
pub mod fixed_register_guard;
pub mod frame_guard;
pub mod target;
pub use compiler_base::{with_compiler_base, CompilerAdaptor, CompilerBase, FunctionSession};

/// Tests only (D398): registers withheld from allocation in every function compiled after it is set (a register mask;
/// 0 = none). The lending tests withhold x18 so Linux runs with the 27 GP registers macOS has (x18 is the platform
/// register there) and the same starved compile is reached on both. Read once per function.
#[doc(hidden)]
pub static TEST_WITHHELD: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
