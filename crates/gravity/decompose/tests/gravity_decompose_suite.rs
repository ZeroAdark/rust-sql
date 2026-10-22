//! gravity_decompose's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_decompose --test gravity_decompose_suite t_<stem>::`.
//! A plain `cargo test -p gravity_decompose` runs every module here in ONE process, so every module must pass beside
//! the others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule):
//! a name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own
//! binary (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state
//! (STATIC_SRCS); a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "agg_aggtab.rs"]
mod t_agg_aggtab;
#[path = "c2_accepted.rs"]
mod t_c2_accepted;
#[path = "c2_exact_proof.rs"]
mod t_c2_exact_proof;
#[path = "core_breaker_row_width.rs"]
mod t_core_breaker_row_width;
#[path = "core_d1071_type_io.rs"]
mod t_core_d1071_type_io;
#[path = "core_d1214.rs"]
mod t_core_d1214;
#[path = "core_d1260.rs"]
mod t_core_d1260;
#[path = "core_d1270.rs"]
mod t_core_d1270;
#[path = "core_decompose.rs"]
mod t_core_decompose;
#[path = "core_leases.rs"]
mod t_core_leases;
#[path = "core_lints.rs"]
mod t_core_lints;
#[path = "core_lives.rs"]
mod t_core_lives;
#[path = "core_node_ops.rs"]
mod t_core_node_ops;
#[path = "core_outs.rs"]
mod t_core_outs;
#[path = "effects_home.rs"]
mod t_effects_home;
#[path = "effects_table.rs"]
mod t_effects_table;
#[path = "explain_order.rs"]
mod t_explain_order;
#[path = "facts_d3347_union_keyed.rs"]
mod t_facts_d3347_union_keyed;
#[path = "facts_d514.rs"]
mod t_facts_d514;
#[path = "facts_index_opc.rs"]
mod t_facts_index_opc;
#[path = "facts_m8.rs"]
mod t_facts_m8;
#[path = "facts_m8b.rs"]
mod t_facts_m8b;
#[path = "facts_rows.rs"]
mod t_facts_rows;
#[path = "facts_typefacts.rs"]
mod t_facts_typefacts;
#[path = "facts_uniq_passenger.rs"]
mod t_facts_uniq_passenger;
#[path = "hashjoin_keys.rs"]
mod t_hashjoin_keys;
#[path = "hashjoin_semi_unread.rs"]
mod t_hashjoin_semi_unread;
#[path = "hashjoin_v4r481_build_rows.rs"]
mod t_hashjoin_v4r481_build_rows;
#[path = "ledger.rs"]
mod t_ledger;
#[path = "lifetime_kept_inputs.rs"]
mod t_lifetime_kept_inputs;
#[path = "lockrows_no_locking_mark.rs"]
mod t_lockrows_no_locking_mark;
#[path = "memoize_purge.rs"]
mod t_memoize_purge;
#[path = "modifytable_partwrite.rs"]
mod t_modifytable_partwrite;
#[path = "modifytable_relset_writes.rs"]
mod t_modifytable_relset_writes;
#[path = "modifytable_write_matrix.rs"]
mod t_modifytable_write_matrix;
#[path = "pg_demand.rs"]
mod t_pg_demand;
#[path = "registered.rs"]
mod t_registered;
#[path = "rerun_reruns.rs"]
mod t_rerun_reruns;
#[path = "scan_datum_text.rs"]
mod t_scan_datum_text;
#[path = "scan_lanes.rs"]
mod t_scan_lanes;
#[path = "setop_setops.rs"]
mod t_setop_setops;
#[path = "sort_record_image_order.rs"]
mod t_sort_record_image_order;
#[path = "sort_w8_no_order_proc.rs"]
mod t_sort_w8_no_order_proc;
#[path = "srf_dispatch_names.rs"]
mod t_srf_dispatch_names;
#[path = "subplan_b4_merge_action.rs"]
mod t_subplan_b4_merge_action;
#[path = "subplan_types.rs"]
mod t_subplan_types;
