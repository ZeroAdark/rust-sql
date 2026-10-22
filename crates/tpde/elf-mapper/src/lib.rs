// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;

mod image;
mod mapping_guard;
pub use image::{MapperImage, Permission};
pub use mapping_guard::{ElfMapper, MappedRange, MappedSymbol};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};

#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}
fn ensure(condition: bool, message: &'static str) -> PgResult<()> {
    if condition {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
