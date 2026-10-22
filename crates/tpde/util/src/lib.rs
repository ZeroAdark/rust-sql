// SPDX-FileCopyrightText: 2025 Contributors to TPDE <https://tpde.org>
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
#![no_std]
#![doc = include_str!("../tests/ownership.md")]
extern crate alloc;
pub mod asan;
mod bitset;
mod bump_guard;
pub mod dwarf;
pub mod function_ref;
pub mod misc;
mod segmented_guard;
mod small_vector_guard;
mod storage;
mod writer_guard;
pub use bitset::SmallBitSet;
pub use bump_guard::{
    BumpAllocArray, BumpAllocUniquePtr, BumpAllocationInfo, BumpAllocator, BumpAllocatorDeleter,
    BumpStats, BUMP_SLAB_ALIGNMENT,
};
pub use segmented_guard::{SegmentedVector, SegmentedVectorGuard};
pub use small_vector_guard::{SmallVectorGuard, UninitResizeGuard};
pub use storage::{default_inline_capacity, SmallVec};
use types_error::{PgError, PgResult, ERRCODE_INVALID_PARAMETER_VALUE};
pub use writer_guard::{Integral, VectorWriter};

/// Upstream `WithAsserts`: `TPDE_ASSERT` is `assert` under `#ifndef NDEBUG`
/// and nothing otherwise. Dev/Miri builds, the `checked` feature and the
/// address-sanitizer stage keep every check; `dist` compiles them out.
pub const CHECKED: bool = cfg!(any(debug_assertions, feature = "checked", tpde_address_sanitizer));

/// `TPDE_ASSERT`: evaluated only when [`CHECKED`]; a dead branch otherwise.
#[macro_export]
macro_rules! tpde_assert {
    ($($args:tt)*) => {
        if $crate::CHECKED {
            ::core::assert!($($args)*);
        }
    };
}
#[macro_export]
macro_rules! tpde_assert_eq {
    ($($args:tt)*) => {
        if $crate::CHECKED {
            ::core::assert_eq!($($args)*);
        }
    };
}
#[macro_export]
macro_rules! tpde_assert_ne {
    ($($args:tt)*) => {
        if $crate::CHECKED {
            ::core::assert_ne!($($args)*);
        }
    };
}
#[cold]
fn invalid(message: &'static str) -> alloc::boxed::Box<PgError> {
    PgError::error(message)
        .with_sqlstate(ERRCODE_INVALID_PARAMETER_VALUE)
        .into()
}
fn ensure(ok: bool, message: &'static str) -> PgResult<()> {
    if ok {
        Ok(())
    } else {
        Err(invalid(message))
    }
}

#[cfg(test)]
extern crate self as tpde_util;
#[cfg(test)]
extern crate std;
#[cfg(all(test, tpde_address_sanitizer))]
#[path = "../../../_support/mcx/tests/asan_reuse/cases.rs"]
mod mcx_asan_reuse;
#[cfg(test)]
#[path = "../tests/small_vector/memory.rs"]
mod small_vector_memory;
#[cfg(test)]
#[path = "../tests/small_vector/unit.rs"]
mod small_vector_tests;
#[cfg(test)]
mod tests;
