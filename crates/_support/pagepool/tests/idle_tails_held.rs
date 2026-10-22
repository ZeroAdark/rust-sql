//! pppress card 10 (`PGRUST_POOL_IDLE_TAIL`), memacct's case (common/idle_held.rs) with no idle cut budget: each pass is
//! one call.
#![cfg(all(feature = "global", target_os = "linux"))]

#[path = "common/idle_held.rs"]
mod case;

#[global_allocator]
static A: pagepool::PoolAlloc = pagepool::PoolAlloc;

#[test]
fn idle_tails_held() {
    case::run(u64::MAX);
}
