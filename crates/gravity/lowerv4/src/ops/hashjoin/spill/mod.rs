//! `ops::hashjoin::spill` (v5 serial step, landing 5: module lines written by modexit.py).
pub(crate) mod breakers;
pub(crate) mod build;
pub(crate) mod forms;
pub(crate) mod pair;
pub(crate) mod raw;
// the pair pipeline's runtime lives under raw/ (rule R6); `spill::load` stays its path
pub(crate) use self::raw::load;
