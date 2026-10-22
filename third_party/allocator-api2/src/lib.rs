//!
//! allocator-api2 crate.
//!
#![cfg_attr(not(feature = "std"), no_std)]
// Vendored ([patch.crates-io] in the workspace Cargo.toml): a path crate gets no --cap-lints, so the lints cargo caps
// for every crates.io dependency are allowed here instead (rustc 1.96 denies dangerous_implicit_autorefs in stable/vec/mod.rs).
#![allow(warnings, dangerous_implicit_autorefs)]

#[cfg(feature = "alloc")]
extern crate alloc as alloc_crate;

#[cfg(not(feature = "nightly"))]
#[macro_use]
mod stable;

#[cfg(feature = "nightly")]
mod nightly;

#[cfg(not(feature = "nightly"))]
pub use self::stable::*;

#[cfg(feature = "nightly")]
pub use self::nightly::*;
