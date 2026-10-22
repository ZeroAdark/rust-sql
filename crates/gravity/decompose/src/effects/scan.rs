//! The effects home's scan half (CONSTRUCT-BUNDLE-RULES, main 2026-10-05: a family's effects moves go in its own sibling
//! file of the home): what a scan's own expressions may show the user when the scan runs again. Moved as is from
//! decompose ops/scan/closure.rs `map_sample` (lint R1, construct bundle scan-01); no behaviour change.
use crate::effects::Volatility;
use crate::rerun::Found;

/// A TABLESAMPLE argument (or REPEATABLE seed) with effects `f` that a scan other than the statement's first fetched
/// node may evaluate again at a rescan (PostgreSQL re-evaluates them at each ExecReScanSampleScan): it shows a difference
/// when it calls VOLATILE code, or when it may raise and reads no executor parameter (D1643: a raising constant argument
/// that PostgreSQL may never reach in a re-run before its first row). `reads_exec`: the argument reads an executor
/// parameter (closure.rs `reads_exec`).
pub(crate) fn sample_arg_loud(f: &Found, reads_exec: bool) -> bool {
    f.volatility == Volatility::Volatile || !(f.raise_free || reads_exec)
}
