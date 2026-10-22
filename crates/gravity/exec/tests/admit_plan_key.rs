use gravity_exec::glue::plan_key;
use gravity_program::{Generation, Handle};
use types_portal::CachedPlanHandle;

#[test]
fn plan_key_is_generic_only_and_packs_the_statement_index_into_the_generation() {
    plancache_portal_seams::plan_identity::set(|c| (7, 42, c.0 == 1));
    assert!(plan_key(CachedPlanHandle::NULL, 0).is_none());
    assert!(plan_key(CachedPlanHandle(2), 0).is_none(), "a custom plan mints no key");
    let k0 = plan_key(CachedPlanHandle(1), 0).expect("generic");
    let k3 = plan_key(CachedPlanHandle(1), 3).expect("generic");
    assert_eq!(k0.handle, Handle(7));
    assert_eq!(k0.generation, Generation(42));
    assert_eq!(k3.handle, k0.handle, "drop_for(handle) covers every statement of the plansource");
    assert_eq!(k3.generation, Generation(42 | 3 << 24));
    assert!(plan_key(CachedPlanHandle(1), 256).is_none());
}
