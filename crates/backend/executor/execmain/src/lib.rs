#![allow(non_snake_case)]

use std::cell::Cell;

use ::mcx::{Mcx, MemoryContext};

mod execami;
mod execcurrent;
mod perms;
mod typefromtl;

pub use execami::{plan_implicit_scroll_ok, plan_supports_backward_scan};
pub use perms::exec_check_one_rel_perms;
pub use typefromtl::{exec_clean_type_from_tl, exec_type_from_tl, expr_collation, expr_typmod};
pub use ::execmain_seams::{
    tap_executor_end, tap_executor_finish, tap_executor_finish_leave, tap_executor_hooks_active,
    tap_executor_run, tap_executor_run_leave, tap_executor_start, HookQd,
};

pub fn with_hook_qd<R>(h: ::types_portal::QueryDescHandle, f: impl FnOnce(&mut ::execmain_seams::HookQd<'_>) -> R) -> R {
    let mut f = Some(f);
    let mut ret: Option<R> = None;
    ::execmain_seams::with_hook_qd::call(h, &mut |v| {
        if let Some(f) = f.take() {
            ret = Some(f(v));
        }
    });
    ret.expect("with_hook_qd: the executor never handed the view")
}

pub fn init_seams() {
    execmain_seams::exec_clean_type_from_tl::set(typefromtl::exec_clean_type_from_tl_seam);
    execmain_seams::exec_check_permissions::set(perms::exec_check_permissions_over_perminfos);
    execmain_seams::exec_current_of::set(execcurrent::exec_current_of_seam);
    // --- WS-CA wave-10 (cursors inc-2, contract §4; escalation EX-CA-1) ---
    execmain_seams::cursor_plan_current_of_eligible::set(
        execcurrent::cursor_plan_current_of_eligible_seam,
    );
    execmain_seams::cursor_plan_rewind_replays::set(execami::cursor_plan_rewind_replays_seam); // D1406
    execmain_seams::cursor_store_fill_enabled::set(|| true);
    execmain_seams::cursor_store_armed_note::set(|| {});
    // --- end WS-CA wave-10 ---
    // --- SE-R41 (reason-41 retirement): the §3.1 capture-batchable probe ---
    execmain_seams::cursor_plan_capture_batch_fill::set(
        execcurrent::cursor_plan_capture_batch_fill_seam,
    );
    // --- end SE-R41 ---
    {
        guc_tables::session_guc_bool!(
            PLP,
            parallel_leader_participation_stand_in,
            set_parallel_leader_participation_stand_in,
            true
        );
        guc_tables::vars::parallel_leader_participation.install_if_absent(
            guc_tables::GucVarAccessors {
                get: parallel_leader_participation_stand_in,
                set: set_parallel_leader_participation_stand_in,
            },
        );
    }
}

// Divergence from C: result tupdescs die in es_query_cxt there (execMain.c),
// with portals copying theirs before ExecutorEnd (portalcmds.c:354); here they
// are refcount-owned — the Rc strong count is the refcount (tupdesc.c model),
// the executor's references drop by ExecutorEnd, and a portal's clone keeps
// its descriptor alive. This backend-lifetime aset only backs those descs;
// TupleDescData's drop pfrees every byte, so the context stays flat per
// statement (desc_context_stays_flat_across_statements).
pub(crate) fn desc_mcx() -> Mcx<'static> {
    thread_local! {
        static CTX: Cell<Option<&'static MemoryContext>> = const { Cell::new(None) };
    }
    CTX.with(|c| {
        let m = match c.get() {
            Some(m) => m,
            None => {
                let m: &'static MemoryContext = ::mcx::session_root("ExecutorResultTypes");
                c.set(Some(m));
                // D4682: the teardown retires this root; clear the slot first, so a reused thread's next
                // session builds a new one.
                ::mcx::register_session_cleanup(Box::new(|| {
                    let _ = CTX.try_with(|c| c.set(None));
                }));
                m
            }
        };
        m.mcx()
    })
}
