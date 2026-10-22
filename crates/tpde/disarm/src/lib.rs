// SPDX-FileCopyrightText: 2024 Alexis Engelke
// SPDX-License-Identifier: BSD-3-Clause
#![no_std]

//! ```compile_fail
//! use tpde_disarm::{de64_ADDx, GRegSp, GRegZr};
//! let sp = GRegSp::new(31).unwrap();
//! let zr = GRegZr::new(31).unwrap();
//! de64_ADDx(sp, zr, zr);
//! ```
//!
//! ```
//! use tpde_disarm::{de64_ADDx, GReg};
//! let r = GReg::new(3).unwrap();
//! assert_ne!(de64_ADDx(r.into(), r.into(), r.into()), 0);
//! ```

mod decode;
mod encode;
mod format;
pub mod runtime;
mod types;

#[allow(non_upper_case_globals, unused_parens)]
pub mod generated {
    include!(concat!(env!("OUT_DIR"), "/public.rs"));
    include!(concat!(env!("OUT_DIR"), "/private.rs"));
}

pub use decode::{da64_classify, da64_decode, decode};
pub use encode::{de64_MOVconst, encode_word, EncodeError};
pub use format::{da64_format, da64_format_abs, FormatError};
pub use generated::*;
pub use runtime::*;
pub use types::*;

pub const UPSTREAM_REVISION: &str = "2d13d3f410a52daff1c5d8ef07d623332f372560";
pub const GENERATION_PROFILE: &str = env!("TPDE_DISARM_PROFILE");
pub const FORM_MANIFEST: &str = include_str!(concat!(env!("OUT_DIR"), "/forms.tsv"));
pub type Da64InstKind = u32;
pub type Da64InstGroup = u32;

const _: () = {
    assert!(!core::mem::needs_drop::<GReg>());
    assert!(!core::mem::needs_drop::<GRegZr>());
    assert!(!core::mem::needs_drop::<GRegSp>());
    assert!(!core::mem::needs_drop::<VReg>());
};
