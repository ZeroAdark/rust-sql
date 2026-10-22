//! `ops::scan::pgrc` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod dict_entries;
pub(crate) mod dict_open;
pub(crate) mod lane_elide;
pub(crate) mod lower;
pub(crate) mod pgrc_late;
pub(crate) mod pgrc_packed;
pub(crate) mod raw;
pub(crate) mod readplan_claim;
pub mod strategies;
pub(crate) mod summary;
pub(crate) mod verdict_allmatch;
pub(crate) mod verdict_bloom;
pub(crate) mod verdict_cond;
pub(crate) mod verdict_cond_key;
pub(crate) mod verdict_psma;
pub(crate) mod verdict_rtconst;
pub(crate) mod verdict_zone;
