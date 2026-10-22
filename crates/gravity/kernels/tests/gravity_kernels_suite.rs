//! gravity_kernels's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_kernels --test gravity_kernels_suite t_<stem>::`.
//! A plain `cargo test -p gravity_kernels` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "agg_bypass_route.rs"]
mod t_agg_bypass_route;
#[path = "agg_cand25_shared_prefix.rs"]
mod t_agg_cand25_shared_prefix;
#[path = "agg_dense_refold.rs"]
mod t_agg_dense_refold;
#[path = "agg_est_load.rs"]
mod t_agg_est_load;
#[path = "agg_fold.rs"]
mod t_agg_fold;
#[path = "agg_m6_agg_home.rs"]
mod t_agg_m6_agg_home;
#[path = "agg_merge_runs.rs"]
mod t_agg_merge_runs;
#[path = "agg_v4_m1.rs"]
mod t_agg_v4_m1;
#[path = "core_d1328_stash_room.rs"]
mod t_core_d1328_stash_room;
#[path = "core_d1340_table_lane.rs"]
mod t_core_d1340_table_lane;
#[path = "core_d1392_image_arena.rs"]
mod t_core_d1392_image_arena;
#[path = "core_d1705_advance_site.rs"]
mod t_core_d1705_advance_site;
#[path = "core_d344_render_error.rs"]
mod t_core_d344_render_error;
#[path = "core_d371_t1_bytes.rs"]
mod t_core_d371_t1_bytes;
#[path = "core_setop_reserved_open.rs"]
mod t_core_setop_reserved_open;
#[path = "core_v4r173_floor_prune.rs"]
mod t_core_v4r173_floor_prune;
#[path = "d2894_floor_t1_commit_first.rs"]
mod t_d2894_floor_t1_commit_first;
#[path = "d2992_zone_cross.rs"]
mod t_d2992_zone_cross;
#[path = "d3027_heads_side_tables.rs"]
mod t_d3027_heads_side_tables;
#[path = "dict_v4_m1_front.rs"]
mod t_dict_v4_m1_front;
#[path = "dict_v4_m3_light.rs"]
mod t_dict_v4_m3_light;
#[path = "group_v4_distinct_window.rs"]
mod t_group_v4_distinct_window;
#[path = "numeric_key_ties.rs"]
mod t_numeric_key_ties;
#[path = "registered.rs"]
mod t_registered;
#[path = "sort_d1494_merge_order.rs"]
mod t_sort_d1494_merge_order;
#[path = "sort_d3141_numeric_sum_words.rs"]
mod t_sort_d3141_numeric_sum_words;
#[path = "sort_d3644_work_blocks.rs"]
mod t_sort_d3644_work_blocks;
#[path = "sort_d364_text_sort_null.rs"]
mod t_sort_d364_text_sort_null;
#[path = "sort_d4411_seal_polls.rs"]
mod t_sort_d4411_seal_polls;
#[path = "sort_d443_count_null_sort.rs"]
mod t_sort_d443_count_null_sort;
#[path = "sort_d456_null_bit_after_text.rs"]
mod t_sort_d456_null_bit_after_text;
#[path = "sort_d466_unsigned_key.rs"]
mod t_sort_d466_unsigned_key;
#[path = "sort_desc_wide_sort.rs"]
mod t_sort_desc_wide_sort;
#[path = "sort_float_words.rs"]
mod t_sort_float_words;
#[path = "sort_hosted_cmp.rs"]
mod t_sort_hosted_cmp;
#[path = "sort_six_keys.rs"]
mod t_sort_six_keys;
#[path = "sort_topn_parthaving_count_slot.rs"]
mod t_sort_topn_parthaving_count_slot;
#[path = "sort_v4_m3.rs"]
mod t_sort_v4_m3;
#[path = "sort_v4_m3_cut.rs"]
mod t_sort_v4_m3_cut;
#[path = "sort_v4r190_decorated_sort.rs"]
mod t_sort_v4r190_decorated_sort;
#[path = "sort_var_keys16.rs"]
mod t_sort_var_keys16;
#[path = "sort_var_keys_far.rs"]
mod t_sort_var_keys_far;
