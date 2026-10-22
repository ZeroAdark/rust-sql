//! gravity_pool's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_pool --test gravity_pool_suite t_<stem>::`.
//! A plain `cargo test -p gravity_pool` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "claim_cut.rs"]
mod t_claim_cut;
#[path = "d4116_temp_fold.rs"]
mod t_d4116_temp_fold;
#[path = "d4242_smgr_release.rs"]
mod t_d4242_smgr_release;
#[path = "d4246_transaction_xmin.rs"]
mod t_d4246_transaction_xmin;
#[path = "maps_temp_road.rs"]
mod t_maps_temp_road;
#[path = "registered.rs"]
mod t_registered;
#[path = "whole_part_claims.rs"]
mod t_whole_part_claims;
