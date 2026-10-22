//! The plancache drop / reset channels into Gravity (docs/gravity/exec.md §5.3).

use gravity_program::Handle;
use types_portal::PlanSourceHandle;

pub fn drop_for(plansource: Option<PlanSourceHandle>) {
    match plansource {
        // D3098: one plansource's entries only, a running level's at its next borrow
        Some(h) => {
            crate::admit::kept::drop_for(Handle(h.0));
            crate::admit::reentry::drop_handle(Handle(h.0));
        }
        None => {
            crate::admit::level_forget();
            crate::admit::kept::clear();
            if crate::admit::reentry::for_each_idle(|b| b.drop_plans(None)) > 0 {
                crate::flush_later();
            }
        }
    }
}

pub fn engine_plan_dropped(plansource: PlanSourceHandle) {
    drop_for(Some(plansource));
}

pub fn engine_plan_cache_reset() {
    drop_for(None);
}

pub fn session_end() {
    crate::admit::reentry::for_each_idle(|b| b.drop_plans(None));
    // the front memo is the session's (a pooled backend thread serves the next session with its own)
    crate::inputs::front::clear();
    crate::teardown();
}
