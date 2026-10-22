//! gravity_hosts's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_hosts --test gravity_hosts_suite t_<stem>::`.
//! A plain `cargo test -p gravity_hosts` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "agg_d580_bracket.rs"]
mod t_agg_d580_bracket;
#[path = "agg_v4_m1.rs"]
mod t_agg_v4_m1;
#[path = "core_d1507_release.rs"]
mod t_core_d1507_release;
#[path = "cost_pgrc_granule.rs"]
mod t_cost_pgrc_granule;
#[path = "dict_code_sparse.rs"]
mod t_dict_code_sparse;
#[path = "group_distinct_fold.rs"]
mod t_group_distinct_fold;
#[path = "group_distinct_fold_order.rs"]
mod t_group_distinct_fold_order;
#[path = "registered.rs"]
mod t_registered;
#[path = "scan_pgrc_answered_whole.rs"]
mod t_scan_pgrc_answered_whole;
#[path = "scan_pgrc_granule_late.rs"]
mod t_scan_pgrc_granule_late;
#[path = "scan_pgrc_merged.rs"]
mod t_scan_pgrc_merged;
#[path = "scan_pgrc_sel_exact_window.rs"]
mod t_scan_pgrc_sel_exact_window;
