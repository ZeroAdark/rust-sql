use mcx::{Mcx, PgVec};
use types_core::Oid;
use types_error::PgResult;
use types_rel::RelationData;
use types_storage::PrepareToInvalidateCacheTuple;
use types_tuple::HeapTupleData;

seam_core::seam!(
    // PrepareToInvalidateCacheTuple (catcache.c) with the per-catcache
    // callback inverted into returned requests (it may lazily initialize a
    // catcache and re-enter inval, so the caller replays outside its borrow).
    pub fn prepare_to_invalidate_cache_tuple<'mcx>(
        mcx: Mcx<'mcx>,
        relation: &RelationData<'_>,
        tuple: &HeapTupleData<'_>,
        newtuple: Option<&HeapTupleData<'_>>,
    ) -> PgResult<PgVec<'mcx, PrepareToInvalidateCacheTuple>>
);

seam_core::seam!(
    pub fn catalog_cache_flush_catalog(cat_id: Oid) -> PgResult<()>
);

seam_core::seam!(
    pub fn reset_catalog_caches_ext(debug_discard: bool) -> PgResult<()>
);

pub static POOL_CATALOG_MISSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[cold]
pub fn note_pool_miss(_cache_id: i32) {
    POOL_CATALOG_MISSES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

std::thread_local! {
    /// Whether this thread registered catalog caches (`catcache::InitCatCache`): a session's thread does, a gravity pool
    /// thread never.
    static CATALOG_HERE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

pub fn note_catalog_registered() {
    CATALOG_HERE.with(|c| c.set(true));
}

pub fn catalog_readable_here() -> bool {
    CATALOG_HERE.try_with(core::cell::Cell::get).unwrap_or(false)
}

pub fn pool_catalog_misses() -> u64 {
    POOL_CATALOG_MISSES.load(core::sync::atomic::Ordering::Relaxed)
}
