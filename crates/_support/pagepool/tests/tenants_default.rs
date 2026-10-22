//! The user 2026-10-18: in the pool-global build `PGRUST_POOL_MCX` and `PGRUST_POOL_GRAVITY` are ON unless `=0` (every gate
//! and no-loss take ran them on); the other switches still need `=1`. Own binary: the switches are read once.
#![cfg(all(feature = "global", not(target_family = "wasm"), not(miri), not(loom)))]

#[test]
fn mcx_and_gravity_default_on_and_zero_turns_one_off() {
    // Edition-2021 std::env::set_var / remove_var (safe fns), the binary's first act, before the pool reads its switches
    std::env::remove_var("PGRUST_POOL_MCX");
    std::env::set_var("PGRUST_POOL_GRAVITY", "0");
    std::env::remove_var("PGRUST_POOL_CACHE");
    std::env::remove_var("PGRUST_ALLOCATOR");
    let s = pagepool::switches();
    assert!(s.mcx, "PGRUST_POOL_MCX unset: on in the pool-global build");
    assert!(!s.gravity, "PGRUST_POOL_GRAVITY=0: off");
    assert!(!s.cache && !s.buffers && !s.keepers && !s.debug, "the other switches still need =1: {s:?}");
    assert!(s.any());
}
