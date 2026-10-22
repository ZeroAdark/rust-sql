//! gravity_abi's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_abi --test gravity_abi_suite t_<stem>::`.
//! A plain `cargo test -p gravity_abi` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "buildrows.rs"]
mod t_buildrows;
#[path = "c6e_limit_exceeded.rs"]
mod t_c6e_limit_exceeded;
#[path = "d1358_var_words.rs"]
mod t_d1358_var_words;
#[path = "d1358b_raw_receivers.rs"]
mod t_d1358b_raw_receivers;
#[path = "d1458_owner_region.rs"]
mod t_d1458_owner_region;
#[path = "d1704_view_align.rs"]
mod t_d1704_view_align;
#[path = "d327_wide_swap.rs"]
mod t_d327_wide_swap;
#[path = "d3762_ptr_status_word.rs"]
mod t_d3762_ptr_status_word;
#[path = "d446_probe_length.rs"]
mod t_d446_probe_length;
#[path = "hashbuild_mix.rs"]
mod t_hashbuild_mix;
#[path = "home_hash.rs"]
mod t_home_hash;
#[path = "host_entry.rs"]
mod t_host_entry;
#[path = "layout.rs"]
mod t_layout;
#[path = "layout_words.rs"]
mod t_layout_words;
#[path = "m6_aggstate.rs"]
mod t_m6_aggstate;
#[path = "owned_totals.rs"]
mod t_owned_totals;
#[path = "rec_face.rs"]
mod t_rec_face;
#[path = "registered.rs"]
mod t_registered;
#[path = "t1_compact.rs"]
mod t_t1_compact;
#[path = "t1_grow.rs"]
mod t_t1_grow;
#[path = "wide.rs"]
mod t_wide;
