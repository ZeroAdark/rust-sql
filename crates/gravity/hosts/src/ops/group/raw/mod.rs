//! The group family's raw half in hosts: every `unsafe` (and lifetime erasure) of the hosted DISTINCT / ORDER BY aggregate
//! forms. `distinct`: the single-input DISTINCT form's run and row / combine / final (the form is ../hosted_distinct.rs);
//! `distinct_multi`: the multi-input DISTINCT fold; `presorted`: the presorted (fed) form; `distinct_fold`: the distinct
//! fold face, re-exported at `ops::group::distinct_fold`.
pub(crate) mod distinct;
pub mod distinct_fold;
pub(crate) mod distinct_multi;
pub(crate) mod presorted;
