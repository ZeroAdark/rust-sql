//! pgrc_qa's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module `t_<stem>`
//! per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package or a crate
//! it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p pgrc_qa --test pgrc_qa_suite t_<stem>::`.
//! A plain `cargo test -p pgrc_qa` runs every module here in ONE process, so every module must pass beside the others.
//! A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a name of
//! ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "clog_fence.rs"]
mod t_clog_fence;
#[path = "dict_gather.rs"]
mod t_dict_gather;
#[path = "framed_view.rs"]
mod t_framed_view;
#[path = "grain_ladder.rs"]
mod t_grain_ladder;
#[path = "granule_edge.rs"]
mod t_granule_edge;
#[path = "kill9_ladder.rs"]
mod t_kill9_ladder;
#[path = "known_issues.rs"]
mod t_known_issues;
#[path = "metamorphic.rs"]
mod t_metamorphic;
#[path = "registered.rs"]
mod t_registered;
#[path = "simvfs_battery.rs"]
mod t_simvfs_battery;
#[path = "wal_checkpoint.rs"]
mod t_wal_checkpoint;
