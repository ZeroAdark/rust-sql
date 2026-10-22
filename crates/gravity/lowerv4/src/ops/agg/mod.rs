//! `ops::agg` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod cascade;
pub(crate) mod dense_small;
pub(crate) mod emit_group_nocode;
pub(crate) mod hash_raise;
pub(crate) mod gsets;
pub(crate) mod gsets_spool;
pub(crate) mod hashagg;
pub(crate) mod premise;
pub(crate) mod raw;
pub mod numeric;
pub(crate) mod run_collapse;
pub mod strategies;
/// The keyed tables' test kit (tests only: no rows), in the family's raw half.
#[cfg(test)]
pub(crate) use raw::testkit;
pub mod trans;
pub(crate) mod walk_skip;
