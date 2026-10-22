//! The hook interfaces (plan §3.1): the variant lists (`kinds`, `roads`, closed at M0) and one face per hook family. Its
//! module lines live here, not in the crate root (rule R9; construct bundle core-face-09).
pub mod consts;
pub mod end;
pub mod kinds;
pub mod policy;
pub mod rewrite;
pub mod runs;
pub mod source;
pub mod spill;
pub mod strategy;
pub mod ext;
pub mod fold;
pub mod step;
pub(crate) mod cosplit;
pub(crate) mod partfill;
pub(crate) mod partruns;
pub(crate) mod pull;
pub(crate) mod slot_run;
pub(crate) mod slot_window;
