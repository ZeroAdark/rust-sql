seam_core::seam!(
    pub fn release_cached_plan(cplan: types_portal::CachedPlanHandle)
);

seam_core::seam!(
    pub fn init_plan_cache() -> types_error::PgResult<()>
);

seam_core::seam!(
    // Extra refcount on a live plan (no C counterpart; C pins via portals
    // only): the executor-skeleton cache pins its parked plan with this.
    pub fn incr_cached_plan(cplan: types_portal::CachedPlanHandle)
);

seam_core::seam!(
    pub fn is_source_generic_plan(cplan: types_portal::CachedPlanHandle) -> bool
);

seam_core::seam!(
    pub fn discard_parked_portal(plansource: types_portal::PlanSourceHandle)
);

seam_core::seam!(
    pub fn plan_identity(cplan: types_portal::CachedPlanHandle) -> (u64, i64, bool)
);

seam_core::seam!(
    pub fn engine_plan_dropped(plansource: types_portal::PlanSourceHandle)
);

seam_core::seam!(
    pub fn engine_plan_cache_reset()
);

seam_core::seam!(
    pub fn engine_portal_dropped(gravity: u64)
);
