//! gravity_tpde's integration tests (tests/<stem>.rs) that can share one process, as ONE test binary: one module
//! `t_<stem>` per file, as lowerv4_suite does for lowerv4's tests. As one binary per file, every edit under the package
//! or a crate it uses compiled and linked each of them again on its own. Now these are one link.
//! ct (scripts/gravity-v5/ct_par.py) runs this binary once per module, in its own process, with that module's exact
//! test names, and keys each result as the former target `<stem>`, so ct keys are the same as with one binary per file.
//! Run one file with `cargo test -p gravity_tpde --test gravity_tpde_suite t_<stem>::`.
//! A plain `cargo test -p gravity_tpde` runs every module here in ONE process, so every module must pass beside the
//! others. A file stays its own [[test]] in Cargo.toml when it cannot share a process (tests/registered.rs's rule): a
//! name of ct's serial tail (cancel, timeout, ...: it runs alone); a #[global_allocator] or a re-run of its own binary
//! (current_exe); text naming its state process-wide or process-global; a `pub static` that holds state (STATIC_SRCS);
//! a seam set or another call that sets or reads process-wide state (PROCESS_STATE, RIG_STATE).
//! tests/registered.rs (the module t_registered; the same guard in every package built this way) checks that every test
//! file is in exactly one of the two places, by these rules.
#[path = "anchor_loop.rs"]
mod t_anchor_loop;
#[path = "asm_debug.rs"]
mod t_asm_debug;
#[path = "coalesce.rs"]
mod t_coalesce;
#[path = "coldcall.rs"]
mod t_coldcall;
#[path = "d398_lend.rs"]
mod t_d398_lend;
#[path = "d414_dead_pred.rs"]
mod t_d414_dead_pred;
#[path = "d4507_outlined.rs"]
mod t_d4507_outlined;
#[path = "demote.rs"]
mod t_demote;
#[path = "disasm.rs"]
mod t_disasm;
#[path = "edge_evict.rs"]
mod t_edge_evict;
#[path = "edge_region.rs"]
mod t_edge_region;
#[path = "fixedregs.rs"]
mod t_fixedregs;
#[path = "func_align.rs"]
mod t_func_align;
#[path = "ops_oracle.rs"]
mod t_ops_oracle;
#[path = "ops_vector.rs"]
mod t_ops_vector;
#[path = "ops_vector_x64.rs"]
mod t_ops_vector_x64;
#[path = "phi_arity.rs"]
mod t_phi_arity;
#[path = "phi_fuzz.rs"]
mod t_phi_fuzz;
#[path = "phi_scratch.rs"]
mod t_phi_scratch;
#[path = "poll_sink.rs"]
mod t_poll_sink;
#[path = "programs.rs"]
mod t_programs;
#[path = "ptr_zero.rs"]
mod t_ptr_zero;
#[path = "registered.rs"]
mod t_registered;
#[path = "reuse.rs"]
mod t_reuse;
#[path = "scangap.rs"]
mod t_scangap;
#[path = "scratch.rs"]
mod t_scratch;
#[path = "stale_entry.rs"]
mod t_stale_entry;
#[path = "v4r153_inner_invariant.rs"]
mod t_v4r153_inner_invariant;
#[path = "view_build_linear.rs"]
mod t_view_build_linear;
#[path = "view_debug.rs"]
mod t_view_debug;
