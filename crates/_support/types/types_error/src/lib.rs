#![no_std]

extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

#[cfg(feature = "std")]
pub mod severity;

pub mod error;
pub mod exceptions;
pub mod pg_error;
pub mod source_map;

pub use error::*;
pub use pg_error::*;
pub use source_map::c_basename_for_rust_path;
