//! The pgrc source's run-time setting reads: read at execution, as PostgreSQL reads a GUC when it acts on it, and never
//! recorded in a program.

pub(crate) fn condition_cache_mode() -> guc_tables::ConditionCacheMode {
    gravity_hosts::inputs::condition_cache_mode()
}

/// `pgrust.condition_cache_unlogged` (boot on): an UNLOGGED pgrc relation's execution takes the
/// condition cache too (a temporary relation never: hosts `GranuleRecipe::cond_unlogged`). Its entries are keyed by the
/// part's uuid and held by the open part, as a permanent relation's; an unlogged relation is reset (emptied to its init
/// fork) only at a server start, in a fresh server process, before any entry exists.
pub(crate) fn condition_cache_unlogged_on() -> bool {
    guc_tables::backing::pgrust_condition_cache_unlogged()
}

/// `pgrust.condition_cache_size` in bytes: the condition cache's capacity, read at every prepare of a site that takes the
/// cache. A set value (kB) as set; the default -1 the budget derived from the machine (`derived_condition_cache_bytes`).
pub(crate) fn condition_cache_bytes() -> u64 {
    match guc_tables::vars::pgrust_condition_cache_size.read() {
        kb if kb >= 0 => kb as u64 * 1024,
        _ => derived_condition_cache_bytes(),
    }
}

/// EX-21: the condition cache's budget at `pgrust.condition_cache_size = -1` (its default), from the machine: its share of
/// the one process-cache budget, min(RAM / 64, the grant pool's capacity), whichever is known (`cost::condition_cache_cap`;
/// the four process caches together stay within 9 / 32 of RAM). The cache's entries are a granule's survivor words each (about
/// 1 KiB), so the budget bounds what a long-lived process keeps across statements and literals, never one statement's
/// use; the flat 100 MiB it replaces held about 70 literals' entries over a 12M-row table, and a workload cycling more
/// missed on every one. SHOW prints the derived budget (`guc_tables::show_pgrust_condition_cache_size`).
pub(crate) fn derived_condition_cache_bytes() -> u64 {
    gravity_hosts::dict::text_hosts::reuse::store_bytes(gravity_program::cost::condition_cache_cap) as u64
}

/// Installs the derivation `guc_tables` reads at -1 (the pgrcolumnar condition cache's arm and SHOW).
pub(crate) fn install_condition_cache_derivation() {
    guc_tables::hooks::derive_pgrust_condition_cache_size.install(derived_condition_cache_bytes);
}
