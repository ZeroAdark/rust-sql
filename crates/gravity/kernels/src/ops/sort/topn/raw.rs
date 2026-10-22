//! `ops::sort::topn`'s raw half in kernels (rule R6: the family's unsafe code lives here): one module per owner face,
//! each moved unchanged from the face's file, which keeps its spec and `owner_fns`.
pub(crate) mod parthaving;
pub(crate) mod t3;
pub mod t3_dense;
pub(crate) mod twopass_top;
