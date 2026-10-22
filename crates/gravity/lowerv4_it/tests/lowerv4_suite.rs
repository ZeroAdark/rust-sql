//! lowerv4's integration tests (crates/gravity/lowerv4/tests/<stem>.rs) as ONE test binary, one module `t_<stem>` per file
//! (BD-21). As 92 binaries, an edit under gravity_exec relinked and rewrote every one of them: on box 16 they were 3,242 of
//! the 6,079 unit-s of that crate-test build, which is write-bound on the target volume. Now it is one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact test
//! names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file. Run
//! one file with `cargo test -p gravity_lowerv4_it --test lowerv4_suite t_<stem>::`.
//! A plain `cargo test -p gravity_lowerv4_it` runs every module in ONE process, so every module here must pass beside the
//! others (main 2026-10-09T20:15:49Z: no false reds). A file stays its own [[test]] in Cargo.toml when it cannot share a
//! process:
//! - it has a #[global_allocator], which would become every module's allocator;
//! - a test re-runs its own binary by exact test name (std::env::current_exe());
//! - it sets process-wide seams or state directly: a `<crate>_seams::<seam>::set(`, pgtz::init_seams /
//!   set_session_timezone, gravity_driver's install_spill_factory, gravity_dict's fp::install;
//! - it pins IR (`ir_pins`), which depends on which seams the process has installed;
//! - it counts the process-wide refusal intern set (`interned()`), which the other modules grow at the same time;
//! - it switches lowerv4's gate stamps (`frame::program::set_gate_stamps`), which every lowering in the process reads.
//! The rig's own Once-guarded installs (RigCatalog::install_seams and the abort-check seam) are the same in every module
//! and stay shared. tests/registered.rs checks that every lowerv4 test file is in exactly one of the two places, by these
//! rules.
#[path = "../../lowerv4/tests/agg_d1391_emit_numkey.rs"]
mod t_agg_d1391_emit_numkey;
#[path = "../../lowerv4/tests/agg_d1417_merge_counts.rs"]
mod t_agg_d1417_merge_counts;
#[path = "../../lowerv4/tests/agg_d384_dup_keys.rs"]
mod t_agg_d384_dup_keys;
#[path = "../../lowerv4/tests/agg_m3_dense_range.rs"]
mod t_agg_m3_dense_range;
#[path = "../../lowerv4/tests/agg_v4r177_walk_skip.rs"]
mod t_agg_v4r177_walk_skip;
#[path = "../../lowerv4/tests/append_d3437_arm_order.rs"]
mod t_append_d3437_arm_order;
#[path = "../../lowerv4/tests/census_func_facts.rs"]
mod t_census_func_facts;
#[path = "../../lowerv4/tests/core_catch.rs"]
mod t_core_catch;
#[path = "../../lowerv4/tests/core_d1029_grow_cancel.rs"]
mod t_core_d1029_grow_cancel;
#[path = "../../lowerv4/tests/core_d1191_rig_seams.rs"]
mod t_core_d1191_rig_seams;
#[path = "../../lowerv4/tests/core_d286_own_cursor.rs"]
mod t_core_d286_own_cursor;
#[path = "../../lowerv4/tests/core_d304_key_coll.rs"]
mod t_core_d304_key_coll;
#[path = "../../lowerv4/tests/core_d343_entries.rs"]
mod t_core_d343_entries;
#[path = "../../lowerv4/tests/core_d478_pull.rs"]
mod t_core_d478_pull;
#[path = "../../lowerv4/tests/core_d482_end_words.rs"]
mod t_core_d482_end_words;
#[path = "../../lowerv4/tests/core_d537_rig_fmgr.rs"]
mod t_core_d537_rig_fmgr;
#[path = "../../lowerv4/tests/core_d638_arena_round.rs"]
mod t_core_d638_arena_round;
#[path = "../../lowerv4/tests/core_d642_grow_bytes.rs"]
mod t_core_d642_grow_bytes;
#[path = "../../lowerv4/tests/core_d645_out_grow.rs"]
mod t_core_d645_out_grow;
#[path = "../../lowerv4/tests/core_d664_spill_epoch.rs"]
mod t_core_d664_spill_epoch;
#[path = "../../lowerv4/tests/core_d795_rig_io.rs"]
mod t_core_d795_rig_io;
#[path = "../../lowerv4/tests/core_diag.rs"]
mod t_core_diag;
#[path = "../../lowerv4/tests/core_emit.rs"]
mod t_core_emit;
#[path = "../../lowerv4/tests/core_m0_faces.rs"]
mod t_core_m0_faces;
#[path = "../../lowerv4/tests/core_m1_frame.rs"]
mod t_core_m1_frame;
#[path = "../../lowerv4/tests/core_mechs_used.rs"]
mod t_core_mechs_used;
#[path = "../../lowerv4/tests/core_rep.rs"]
mod t_core_rep;
#[path = "../../lowerv4/tests/d407_literal_free.rs"]
mod t_d407_literal_free;
#[path = "../../lowerv4/tests/dump_fixture.rs"]
mod t_dump_fixture;
#[path = "../../lowerv4/tests/effects_guard_user_fn.rs"]
mod t_effects_guard_user_fn;
#[path = "../../lowerv4/tests/expr_r336_anti_junk.rs"]
mod t_expr_r336_anti_junk;
#[path = "../../lowerv4/tests/group_d2855_absorb_keeps_rewind.rs"]
mod t_group_d2855_absorb_keeps_rewind;
#[path = "../../lowerv4/tests/hashjoin_d1486_joinarms.rs"]
mod t_hashjoin_d1486_joinarms;
#[path = "../../lowerv4/tests/hashjoin_d3168_memo_program.rs"]
mod t_hashjoin_d3168_memo_program;
#[path = "../../lowerv4/tests/hashjoin_m1_join.rs"]
mod t_hashjoin_m1_join;
#[path = "../../lowerv4/tests/hashjoin_m7_l0a.rs"]
mod t_hashjoin_m7_l0a;
#[path = "../../lowerv4/tests/keysem_image_fixture.rs"]
mod t_keysem_image_fixture;
#[path = "../../lowerv4/tests/lower_insns_guard.rs"]
mod t_lower_insns_guard;
#[path = "../../lowerv4/tests/m0_keyhash_only.rs"]
mod t_m0_keyhash_only;
#[path = "../../lowerv4/tests/material_d1108_quiet.rs"]
mod t_material_d1108_quiet;
#[path = "../../lowerv4/tests/material_d1633_spool_pairs.rs"]
mod t_material_d1633_spool_pairs;
#[path = "../../lowerv4/tests/material_d1919_readers.rs"]
mod t_material_d1919_readers;
#[path = "../../lowerv4/tests/material_m8_store_reopen.rs"]
mod t_material_m8_store_reopen;
#[path = "../../lowerv4/tests/mergejoin_m11.rs"]
mod t_mergejoin_m11;
#[path = "../../lowerv4/tests/modifytable_d779_onconflict_where.rs"]
mod t_modifytable_d779_onconflict_where;
#[path = "../../lowerv4/tests/modifytable_m4.rs"]
mod t_modifytable_m4;
#[path = "../../lowerv4/tests/modifytable_m4_l0.rs"]
mod t_modifytable_m4_l0;
#[path = "../../lowerv4/tests/modifytable_m4_route.rs"]
mod t_modifytable_m4_route;
#[path = "../../lowerv4/tests/modifytable_m4_trig.rs"]
mod t_modifytable_m4_trig;
#[path = "../../lowerv4/tests/nestloop_d3014o_widen.rs"]
mod t_nestloop_d3014o_widen;
#[path = "../../lowerv4/tests/nestloop_m8_embedded_inner_index.rs"]
mod t_nestloop_m8_embedded_inner_index;
#[path = "../../lowerv4/tests/nestloop_m8_l0w.rs"]
mod t_nestloop_m8_l0w;
#[path = "../../lowerv4/tests/props_needs.rs"]
mod t_props_needs;
#[path = "../../lowerv4/tests/result_cap29_values_stream.rs"]
mod t_result_cap29_values_stream;
#[path = "../../lowerv4/tests/result_new29_values_threshold.rs"]
mod t_result_new29_values_threshold;
#[path = "../../lowerv4/tests/roads_body_form_general.rs"]
mod t_roads_body_form_general;
#[path = "../../lowerv4/tests/roads_prefetch_fp_general.rs"]
mod t_roads_prefetch_fp_general;
#[path = "../../lowerv4/tests/scan_d509_prune.rs"]
mod t_scan_d509_prune;
#[path = "../../lowerv4/tests/scan_d665_pruned_root.rs"]
mod t_scan_d665_pruned_root;
#[path = "../../lowerv4/tests/scan_m2_distinct_count_alone.rs"]
mod t_scan_m2_distinct_count_alone;
#[path = "../../lowerv4/tests/scan_m2_index.rs"]
mod t_scan_m2_index;
#[path = "../../lowerv4/tests/scan_sortmerge_t2.rs"]
mod t_scan_sortmerge_t2;
#[path = "../../lowerv4/tests/sort_d370_span_groups.rs"]
mod t_sort_d370_span_groups;
#[path = "../../lowerv4/tests/sort_d643_topk_finish.rs"]
mod t_sort_d643_topk_finish;
#[path = "../../lowerv4/tests/sort_m3_arms_sort.rs"]
mod t_sort_m3_arms_sort;
#[path = "../../lowerv4/tests/sort_m3_l0.rs"]
mod t_sort_m3_l0;
#[path = "../../lowerv4/tests/sort_m3_owner.rs"]
mod t_sort_m3_owner;
#[path = "../../lowerv4/tests/sort_m3_walk.rs"]
mod t_sort_m3_walk;
#[path = "../../lowerv4/tests/srf_m5_class_keyed.rs"]
mod t_srf_m5_class_keyed;
#[path = "../../lowerv4/tests/srf_m5_l0_srf_args.rs"]
mod t_srf_m5_l0_srf_args;
#[path = "../../lowerv4/tests/srf_m5_l0_srf_cells.rs"]
mod t_srf_m5_l0_srf_cells;
#[path = "../../lowerv4/tests/srf_m5_l0_srf_parallel.rs"]
mod t_srf_m5_l0_srf_parallel;
#[path = "../../lowerv4/tests/srf_m5_pl.rs"]
mod t_srf_m5_pl;
#[path = "../../lowerv4/tests/srf_m5_series.rs"]
mod t_srf_m5_series;
#[path = "../../lowerv4/tests/srf_v4r233_srf_pl_lang.rs"]
mod t_srf_v4r233_srf_pl_lang;
#[path = "../../lowerv4/tests/subplan_d1415_window_subplan_arg.rs"]
mod t_subplan_d1415_window_subplan_arg;
#[path = "../../lowerv4/tests/subplan_d1478_subplan_rescan.rs"]
mod t_subplan_d1478_subplan_rescan;
#[path = "../../lowerv4/tests/subplan_m8_d1085_once.rs"]
mod t_subplan_m8_d1085_once;
#[path = "../../lowerv4/tests/subplan_m8_initplan.rs"]
mod t_subplan_m8_initplan;
#[path = "../../lowerv4/tests/subplan_m8_subhash_lazy_consumes.rs"]
mod t_subplan_m8_subhash_lazy_consumes;
#[path = "../../lowerv4/tests/topn_dc04_group_top_region.rs"]
mod t_topn_dc04_group_top_region;
