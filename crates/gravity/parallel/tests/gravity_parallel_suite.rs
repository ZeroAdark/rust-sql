//! gravity_parallel's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_parallel --test gravity_parallel_suite t_<stem>::`.
//! A plain `cargo test -p gravity_parallel` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "d1702_unwind.rs"]
mod t_d1702_unwind;
#[path = "d1708_prerun_derive.rs"]
mod t_d1708_prerun_derive;
#[path = "d276_run_empty.rs"]
mod t_d276_run_empty;
#[path = "d292_unopened_finish.rs"]
mod t_d292_unopened_finish;
#[path = "d4033_partition_counts.rs"]
mod t_d4033_partition_counts;
#[path = "dag.rs"]
mod t_dag;
#[path = "error.rs"]
mod t_error;
#[path = "kfold.rs"]
mod t_kfold;
#[path = "kway.rs"]
mod t_kway;
#[path = "op14_defer.rs"]
mod t_op14_defer;
#[path = "registered.rs"]
mod t_registered;
#[path = "scan_count.rs"]
mod t_scan_count;
#[path = "stage.rs"]
mod t_stage;
