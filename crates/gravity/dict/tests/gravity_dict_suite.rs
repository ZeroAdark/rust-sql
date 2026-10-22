//! gravity_dict's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_dict --test gravity_dict_suite t_<stem>::`.
//! A plain `cargo test -p gravity_dict` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "code_set.rs"]
mod t_code_set;
#[path = "collision.rs"]
mod t_collision;
#[path = "d319_once.rs"]
mod t_d319_once;
#[path = "deferral.rs"]
mod t_deferral;
#[path = "hash_quality.rs"]
mod t_hash_quality;
#[path = "like_corpus.rs"]
mod t_like_corpus;
#[path = "like_payload.rs"]
mod t_like_payload;
#[path = "regexp_linear.rs"]
mod t_regexp_linear;
#[path = "registered.rs"]
mod t_registered;
#[path = "scan_fit.rs"]
mod t_scan_fit;
