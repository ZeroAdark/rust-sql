//! `ops::mergejoin` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod exact_inner;
pub(crate) mod fed_first;
pub(crate) mod fill_inner;
pub(crate) mod full_whole;
pub(crate) mod held_inner;
pub(crate) mod lazy_outer;
pub(crate) mod lower;
pub(crate) mod machine;
pub(crate) mod nest_inner;
pub(crate) mod nest_refill;
pub(crate) mod premise;
pub(crate) mod spool_inner;
pub mod strategies;
