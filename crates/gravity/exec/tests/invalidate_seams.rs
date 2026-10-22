//! D3059: gravity_exec's init_seams installs plancache's drop / reset channels (invalidate.rs), so a DEALLOCATE / a
//! prepared statement's teardown (DropCachedPlan) forgets the dropped plansource's entries on every idle level, and a
//! reset (DISCARD PLANS / ALL, ResetPlanCache) clears each level's memo. SQE's statement memo held both slots until its
//! deletion, which left them empty: the entries of dropped plans stayed until the session's end.

use gravity_exec::admit::reentry;
use gravity_exec::{Backend, Verdict};
use gravity_program::{Generation, Handle, PlanKey};

fn build(_depth: usize) -> Box<Backend> {
    Box::new(Backend::new_for_test()) // alloc: Cold
}

fn key(handle: u64) -> PlanKey {
    PlanKey { handle: Handle(handle), generation: Generation(1) }
}

/// init_seams installs process-wide slots once (a second install panics): the tests share one call.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(gravity_exec::init_seams);
}

fn lookup(k: PlanKey) -> Option<Verdict> {
    reentry::enter(None, None, false, |b, _| b.lookup(k)).expect("level").expect("built")
}

#[test]
fn init_seams_installs_the_drop_and_reset_channels() {
    init();
    assert!(plancache_portal_seams::engine_plan_dropped::is_installed(), "DropCachedPlan reaches Gravity");
    assert!(plancache_portal_seams::engine_plan_cache_reset::is_installed(), "ResetPlanCache reaches Gravity");
    // a level remembers two plansources' verdicts
    reentry::enter(Some(build), None, false, |b, _| {
        b.remember_refusal(key(0x40));
        b.remember_refusal(key(0x41));
    })
    .expect("level");
    assert_eq!(lookup(key(0x40)), Some(Verdict::Refused));
    // DEALLOCATE of 0x40: its entry goes, 0x41's stays
    plancache_portal_seams::engine_plan_dropped::call(types_portal::PlanSourceHandle(0x40));
    assert_eq!(lookup(key(0x40)), None, "the dropped plansource's verdict is forgotten");
    assert_eq!(lookup(key(0x41)), Some(Verdict::Refused));
    // DISCARD PLANS: every entry goes
    plancache_portal_seams::engine_plan_cache_reset::call();
    assert_eq!(lookup(key(0x41)), None, "a reset clears the level's memo");
    reentry::teardown();
}

/// D3098: a plansource dropped while a level runs (a plpgsql EXECUTE's one-shot plan, dropped inside the calling
/// statement) leaves that level's other entries alone: the level drops the one handle at its next borrow, never a whole
/// flush (which threw away every program of the level each call: rt_app-027x +2.5 ms a statement).
#[test]
fn a_drop_under_a_running_level_forgets_only_that_plansource() {
    init();
    reentry::enter(Some(build), None, false, |b, _| {
        b.remember_refusal(key(0x50));
        b.remember_refusal(key(0x51));
    })
    .expect("level");
    // level 0 runs; the nested statement's one-shot plansource 0x50 is dropped inside it
    reentry::enter(Some(build), None, false, |_, depth| {
        assert_eq!(depth, 0);
        plancache_portal_seams::engine_plan_dropped::call(types_portal::PlanSourceHandle(0x50));
    })
    .expect("level");
    assert_eq!(lookup(key(0x50)), None, "the dropped plansource's verdict is forgotten at the level's next borrow");
    assert_eq!(lookup(key(0x51)), Some(Verdict::Refused), "the running level's other entries stay (no whole flush)");
    reentry::teardown();
}
