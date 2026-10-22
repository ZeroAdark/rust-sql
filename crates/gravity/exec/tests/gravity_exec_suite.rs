//! gravity_exec's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_exec --test gravity_exec_suite t_<stem>::`.
//! A plain `cargo test -p gravity_exec` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "admit_entry_d1216_serve_size.rs"]
mod t_admit_entry_d1216_serve_size;
#[path = "admit_entry_handles.rs"]
mod t_admit_entry_handles;
#[path = "admit_memo.rs"]
mod t_admit_memo;
#[path = "admit_regen.rs"]
mod t_admit_regen;
#[path = "admit_runner.rs"]
mod t_admit_runner;
#[path = "agg_d271_plain_budget.rs"]
mod t_agg_d271_plain_budget;
#[path = "d366_drain.rs"]
mod t_d366_drain;
#[path = "d377_drain_tail.rs"]
mod t_d377_drain_tail;
#[path = "d379_claim_poll.rs"]
mod t_d379_claim_poll;
#[path = "entry_routes_every_handle_seam.rs"]
mod t_entry_routes_every_handle_seam;
#[path = "hashjoin_d262_spill_steps.rs"]
mod t_hashjoin_d262_spill_steps;
#[path = "inputs_front_d1103_mod.rs"]
mod t_inputs_front_d1103_mod;
#[path = "inputs_front_d644.rs"]
mod t_inputs_front_d644;
#[path = "inputs_front_div_by_const.rs"]
mod t_inputs_front_div_by_const;
#[path = "inputs_front_e2_textpos.rs"]
mod t_inputs_front_e2_textpos;
#[path = "inputs_front_lit_guard.rs"]
mod t_inputs_front_lit_guard;
#[path = "inputs_front_nest_params.rs"]
mod t_inputs_front_nest_params;
#[path = "inputs_front_uniq.rs"]
mod t_inputs_front_uniq;
#[path = "owner_fold_jit.rs"]
mod t_owner_fold_jit;
#[path = "rearm_e2_lives_answers.rs"]
mod t_rearm_e2_lives_answers;
#[path = "registered.rs"]
mod t_registered;
#[path = "srf_m5_srf_acl.rs"]
mod t_srf_m5_srf_acl;
#[path = "table_d597_domain_miss.rs"]
mod t_table_d597_domain_miss;
#[path = "table_owner.rs"]
mod t_table_owner;
#[path = "table_owner_dense.rs"]
mod t_table_owner_dense;
#[path = "table_owner_key_check.rs"]
mod t_table_owner_key_check;
#[path = "table_owner_observe_key.rs"]
mod t_table_owner_observe_key;
