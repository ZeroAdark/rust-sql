//! pppress card 10b: memacct's case (common/idle_held.rs) at an idle cut budget of 0, so the budget stops every pass at
//! each run after its first and the pass's TAIL_AGAIN reruns rest on the deferrals carried across its resumes.
#![cfg(all(feature = "global", target_os = "linux"))]

#[path = "common/idle_held.rs"]
mod case;

#[global_allocator]
static A: pagepool::PoolAlloc = pagepool::PoolAlloc;

#[test]
fn idle_tails_held_budgeted() {
    case::run(0);
}
