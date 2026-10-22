//! gravity_driver's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_driver --test gravity_driver_suite t_<stem>::`.
//! A plain `cargo test -p gravity_driver` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "c9_stopped_call.rs"]
mod t_c9_stopped_call;
#[path = "claim_order.rs"]
mod t_claim_order;
#[path = "claim_panic.rs"]
mod t_claim_panic;
#[path = "cost_machine.rs"]
mod t_cost_machine;
#[path = "d1339_rerun_pins.rs"]
mod t_d1339_rerun_pins;
#[path = "d1706_bind_provenance.rs"]
mod t_d1706_bind_provenance;
#[path = "d1707_claim_binding.rs"]
mod t_d1707_claim_binding;
#[path = "d1712_worker_grow.rs"]
mod t_d1712_worker_grow;
#[path = "d1713_leader_grow.rs"]
mod t_d1713_leader_grow;
#[path = "d1876_nested_out_rows.rs"]
mod t_d1876_nested_out_rows;
#[path = "d2224_ended_sites.rs"]
mod t_d2224_ended_sites;
#[path = "d2378_nested_claim.rs"]
mod t_d2378_nested_claim;
#[path = "d314_unwind.rs"]
mod t_d314_unwind;
#[path = "d3215_worker_floor.rs"]
mod t_d3215_worker_floor;
#[path = "d3244_parked_pull_window.rs"]
mod t_d3244_parked_pull_window;
#[path = "d327_wide_carry.rs"]
mod t_d327_wide_carry;
#[path = "d3294_consumed_not_live.rs"]
mod t_d3294_consumed_not_live;
#[path = "d365_estimate.rs"]
mod t_d365_estimate;
#[path = "d3957_refused_image.rs"]
mod t_d3957_refused_image;
#[path = "d4030_worker_claim_abort.rs"]
mod t_d4030_worker_claim_abort;
#[path = "d4101_nested_fed_hold.rs"]
mod t_d4101_nested_fed_hold;
#[path = "d4800_lazy_build_raises.rs"]
mod t_d4800_lazy_build_raises;
#[path = "d699_recheck_push.rs"]
mod t_d699_recheck_push;
#[path = "early_stop.rs"]
mod t_early_stop;
#[path = "entry.rs"]
mod t_entry;
#[path = "exactsize_b1a_grow_exact.rs"]
mod t_exactsize_b1a_grow_exact;
#[path = "held_sites.rs"]
mod t_held_sites;
#[path = "hostcall.rs"]
mod t_hostcall;
#[path = "identity.rs"]
mod t_identity;
#[path = "l8_section.rs"]
mod t_l8_section;
#[path = "multilock.rs"]
mod t_multilock;
#[path = "park_total.rs"]
mod t_park_total;
#[path = "partition_cap.rs"]
mod t_partition_cap;
#[path = "pull.rs"]
mod t_pull;
#[path = "pull_width.rs"]
mod t_pull_width;
#[path = "recheck.rs"]
mod t_recheck;
#[path = "registered.rs"]
mod t_registered;
#[path = "revoke_poll_unbound.rs"]
mod t_revoke_poll_unbound;
#[path = "site_rows_fold.rs"]
mod t_site_rows_fold;
#[path = "t1_destage.rs"]
mod t_t1_destage;
#[path = "tier0.rs"]
mod t_tier0;
