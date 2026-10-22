//! gravity_lint's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_lint --test gravity_lint_suite t_<stem>::`.
//! A plain `cargo test -p gravity_lint` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "alloc_sites.rs"]
mod t_alloc_sites;
#[path = "asm_loop.rs"]
mod t_asm_loop;
#[path = "deps_thread.rs"]
mod t_deps_thread;
#[path = "env_reads.rs"]
mod t_env_reads;
#[path = "fixture_pool.rs"]
mod t_fixture_pool;
#[path = "front_bake_lint.rs"]
mod t_front_bake_lint;
#[path = "hand_kernels_gone.rs"]
mod t_hand_kernels_gone;
#[path = "host_decls.rs"]
mod t_host_decls;
#[path = "kernel_boundary.rs"]
mod t_kernel_boundary;
#[path = "no_baked_manifest_dir.rs"]
mod t_no_baked_manifest_dir;
#[path = "registered.rs"]
mod t_registered;
#[path = "root_is_the_checkout.rs"]
mod t_root_is_the_checkout;
