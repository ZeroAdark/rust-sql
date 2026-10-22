//! `table` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod emit;
pub mod entries;
pub mod fold_jit;
pub(crate) mod fold_entries;
pub mod image;
pub(crate) mod img_reclaim;
pub mod owner_fold;
pub mod params;
pub mod route;
pub use emit::EMIT_KEPT_OFF;
pub use emit::EMIT_PTRS_OFF;
/// The admit header and its flag words, read across the kernel boundary (lowering and exec build or read the header the
/// hash-aggregate entries keep).
pub use entries::{AdmitHdr, ADMIT_ALL, ADMIT_EMPTY, ADMIT_EMPTY_KEY, ADMIT_FILTER, ADMIT_FILTER_AT, ADMIT_FILTER_LEN, ADMIT_MASK, ADMIT_SET};
