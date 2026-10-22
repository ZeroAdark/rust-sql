#![allow(non_snake_case)]
#![allow(non_upper_case_globals)]

pub mod compute;
mod graph;
mod init;
mod inval;
mod l2;
mod list;
mod search;
pub mod testing;
#[cfg(test)]
mod tests;

use core::cell::UnsafeCell;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use datum::Datum;
use mcx::{bind, Mcx, McxOwned, MemoryContext, PgString, PgVec};
use types_core::Oid;
use types_tuple::{ItemPointerData, TupleDescData};

pub use compute::{CatCKey, CCFastKind, CATCACHE_MAXKEYS};
pub use graph::{
    inval_epoch, CatCacheInvalidate, CatalogCacheFlushCatalog, InitCatCache, ResetCatalogCaches,
    ResetCatalogCachesExt, SetCatCacheTrimAtIdle, TrimCatCacheAtIdle,
};
pub use init::{cache_nkeys, cache_relisshared, cache_tupdesc, InitCatCachePhase2};
pub use inval::PrepareToInvalidateCacheTuple;
pub use list::{CatCList as CatCListRef, ReleaseCatCacheList, SearchCatCacheList};
pub use search::{
    CatCTuple, GetCatCacheHashValue, ReleaseCatCache, SearchCatCache, SearchCatCache1,
    SearchCatCache2, SearchCatCache3, SearchCatCache4,
};

pub(crate) const NONE: u32 = u32::MAX;

pub(crate) struct CatCTup {
    pub hash_value: u32,
    pub refcount: i32,
    pub dead: bool,
    pub negative: bool,
    pub hot: bool,
    pub next: u32,
    pub prev: u32,
    pub c_list: u32,
    pub keys: [Datum; CATCACHE_MAXKEYS],
    pub t_len: u32,
    pub t_self: ItemPointerData,
    pub t_tableoid: Oid,
    pub payload: *mut u8,
    pub payload_len: u32,
    pub shared: Option<std::sync::Arc<l2::CatL2Entry>>,
}

pub(crate) const IMG_PREFIX: usize = 16;

impl CatCTup {
    #[inline(always)]
    pub(crate) fn image_ptr(&self) -> *mut u8 {
        debug_assert!(!self.negative && !self.payload.is_null());
        // SAFETY: positive-entry payloads start with IMG_PREFIX + t_len bytes.
        unsafe { self.payload.add(IMG_PREFIX) }
    }
}

const _: () = assert!(core::mem::size_of::<CatCTup>() <= 128);

pub(crate) struct CatCList<'mcx> {
    pub hash_value: u32,
    pub refcount: i32,
    pub dead: bool,
    pub ordered: bool,
    pub nkeys: i16,
    pub next: u32,
    pub prev: u32,
    pub keys: [Datum; CATCACHE_MAXKEYS],
    pub payload: *mut u8,
    pub payload_len: u32,
    pub members: PgVec<'mcx, u32>,
}

pub(crate) struct CatCache<'mcx> {
    pub id: i32,
    pub cc_reloid: Oid,
    pub cc_indexoid: Oid,
    pub cc_relisshared: bool,
    pub initialized: bool,
    pub cc_ntup: i32,
    pub cc_nlist: i32,
    pub cc_nbuckets: u32,
    pub cc_nlbuckets: u32,
    pub cc_nkeys: i32,
    pub cc_keyno: [i32; CATCACHE_MAXKEYS],
    pub cc_kind: [CCFastKind; CATCACHE_MAXKEYS],
    pub cc_eqfunc: [Oid; CATCACHE_MAXKEYS],
    pub cc_tupdesc: Option<&'static TupleDescData<'static>>,
    pub cc_relname: Option<PgString<'mcx>>,
    pub cc_bucket: PgVec<'mcx, u32>,
    pub cc_lbucket: PgVec<'mcx, u32>,
    pub tuples: PgVec<'mcx, CatCTup>,
    pub ct_free: u32,
    pub lists: PgVec<'mcx, CatCList<'mcx>>,
    pub cl_free: u32,
}

#[derive(Clone, Copy)]
pub(crate) struct CatCInProgress {
    pub cache_id: i32,
    pub hash_value: u32,
    pub list: bool,
    pub dead: bool,
}

pub(crate) struct CatCacheState<'mcx> {
    pub mcx: Mcx<'mcx>,
    pub caches: PgVec<'mcx, Option<CatCache<'mcx>>>,
    pub ch_ntup: i32,
    pub ch_nlisted: i32,
    pub cap_retry_at: i32,
    pub in_progress: PgVec<'mcx, CatCInProgress>,
    pub clock_cache: usize,
    pub clock_slot: usize,
}

/// C has no counterpart — the C catcache grows without bound for the life of
/// the process. Evicting an unpinned entry is semantically identical to that
/// entry's invalidation arriving while unpinned (CatCacheInvalidate removes
/// refcount==0 entries outright): the next probe re-reads the catalog.
pub(crate) fn catcache_cap() -> i32 {
    #[cfg(test)]
    if let Some(v) = CAP_OVERRIDE.with(core::cell::Cell::get) {
        return v;
    }
    static ENV: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
    if let Some(v) = *ENV.get_or_init(|| {
        std::env::var("PGRUST_CATCACHE_CAP").ok().and_then(|v| v.trim().parse().ok())
    }) {
        return v;
    }
    guc_tables::backing::catcache_size_limit()
}

#[cfg(test)]
thread_local! {
    pub(crate) static CAP_OVERRIDE: core::cell::Cell<Option<i32>> = const { core::cell::Cell::new(None) };
}

bind!(pub(crate) CatCacheStateTy => CatCacheState<'mcx>);

thread_local! {
    // UnsafeCell, not RefCell: borrow-flag traffic on every catalog lookup
    // is overhead C does not pay (an internal issue, ~12% suite-wide).
    static STATE: UnsafeCell<Option<ManuallyDrop<McxOwned<CatCacheStateTy>>>> =
        const { UnsafeCell::new(None) };
}

#[cfg(debug_assertions)]
thread_local! {
    static BORROW_DEPTH: core::cell::Cell<u8> = const { core::cell::Cell::new(0) };
    /// this thread's pool claim runs hosted sites admitted on their catalog arm: a miss is a bug.
    static POOL_STRICT: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

pub fn set_pool_strict(on: bool) -> bool {
    #[cfg(debug_assertions)]
    {
        POOL_STRICT.try_with(|c| c.replace(on)).unwrap_or(false)
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = on;
        false
    }
}

#[cfg(debug_assertions)]
struct BorrowGuard;

#[cfg(debug_assertions)]
impl Drop for BorrowGuard {
    fn drop(&mut self) {
        BORROW_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

#[cold]
fn state_init(slot: &mut Option<ManuallyDrop<McxOwned<CatCacheStateTy>>>) {
    let cache_context = MemoryContext::new("CacheMemoryContext");
    cache_context.set_owner_session();
    let owned = McxOwned::<CatCacheStateTy>::try_new(
        cache_context,
        |mcx| {
            Ok(CatCacheState {
                mcx,
                caches: PgVec::new_in(mcx),
                ch_ntup: 0,
                ch_nlisted: 0,
                cap_retry_at: 0,
                in_progress: PgVec::new_in(mcx),
                clock_cache: 0,
                clock_slot: 0,
            })
        },
    )
    .expect("CacheMemoryContext allocation");
    *slot = Some(ManuallyDrop::new(owned));
    // C frees the whole catcache with the backend process; the thread model
    // frees it here or every session leaks its catalog-cache estate into the
    // shared process.
    ::mcx::register_session_cleanup(Box::new(|| {
        STATE.with(|cell| {
            // SAFETY: task-end teardown, outside any catcache borrow.
            if let Some(mut owned) = unsafe { &mut *cell.get() }.take() {
                owned.with_mut(|st| {
                    for cache in st.caches.iter_mut().flatten() {
                        if let Some(td) = cache.cc_tupdesc.take() {
                            // SAFETY: `td` is the Box::leak'd header from
                            // catalog_cache_initialize_cache, taken out of
                            // its only holder just now; nothing reads a
                            // cache after this cleanup on this thread.
                            unsafe {
                                drop(Box::from_raw(
                                    td as *const TupleDescData<'static> as *mut TupleDescData<'static>,
                                ));
                            }
                        }
                    }
                });
                drop(ManuallyDrop::into_inner(owned));
            }
        });
    }));
}

/// A pin dropping after session teardown has nothing left to release.
pub(crate) fn with_live_state(f: impl for<'mcx> FnOnce(&mut CatCacheState<'mcx>)) {
    // SAFETY: a plain liveness read of this thread's slot, outside any borrow.
    let live = STATE.try_with(|cell| unsafe { (*cell.get()).is_some() }).unwrap_or(false);
    if live {
        with_state(f);
    }
}

/// # Safety
///
/// Never re-entered while a borrow is live: one single-threaded backend owns
/// the thread-local; every operation confines its borrow to one `f` and
/// drops it before any call that can re-enter (init, miss scan, callbacks).
/// A pure hit calls no foreign code inside `f`. The debug/Miri guard turns
/// any violation into a panic.
#[inline(always)]
pub(crate) fn with_state<R>(f: impl for<'mcx> FnOnce(&mut CatCacheState<'mcx>) -> R) -> R {
    // Tiny closure: LocalKey::with outlines big ones (an extra call frame).
    let cell = STATE.with(|cell| cell as *const UnsafeCell<Option<ManuallyDrop<McxOwned<CatCacheStateTy>>>>);
    #[cfg(debug_assertions)]
    let _guard = {
        BORROW_DEPTH.with(|d| {
            assert_eq!(d.get(), 0, "catcache state re-entered while a borrow is live");
            d.set(1);
        });
        BorrowGuard
    };
    // SAFETY: `cell` is this thread's own TLS slot, used only within this
    // call; single-statement, non-reentrant borrow (see above).
    let slot = unsafe { &mut *(*cell).get() };
    if slot.is_none() {
        state_init(slot);
    }
    slot.as_mut().unwrap().with_mut(f)
}

/// V4R-453: a search of a cache this thread never registered, a named ERROR instead of `cache`'s panic: a gravity
/// pool thread has no catcache, so a lookup its claim's catalog arm (typcache / lsyscache, resolved on the leader) did not
/// cover lands here, and the ERROR travels the pool's error path, where a panic took the server down.
#[cold]
#[inline(never)]
pub(crate) fn unregistered(id: i32) -> Box<types_error::PgError> {
    catcache_seams::note_pool_miss(id);
    #[cfg(debug_assertions)]
    debug_assert!(!POOL_STRICT.try_with(core::cell::Cell::get).unwrap_or(false), "B2: catalog cache {id} missed on a pool claim whose hosted sites were admitted on their catalog arm");
    Box::new(types_error::PgError::error(format!(
        "catcache: cache id {id} is not registered on this thread (a gravity pool worker has no catcache: a catalog fact not resolved on the leader, D1409)"
    )))
}

impl<'mcx> CatCacheState<'mcx> {
    /// Whether cache `id` is registered on this thread (the search entries' check, V4R-453).
    #[inline]
    pub(crate) fn registered(&self, id: i32) -> bool {
        self.caches.get(id as usize).is_some_and(|c| c.is_some())
    }

    #[inline]
    pub(crate) fn cache(&self, id: i32) -> &CatCache<'mcx> {
        self.caches
            .get(id as usize)
            .and_then(|c| c.as_ref())
            .unwrap_or_else(|| panic!("catcache: cache id {id} not registered"))
    }

    #[inline]
    pub(crate) fn cache_mut(&mut self, id: i32) -> &mut CatCache<'mcx> {
        self.caches
            .get_mut(id as usize)
            .and_then(|c| c.as_mut())
            .unwrap_or_else(|| panic!("catcache: cache id {id} not registered"))
    }
}

#[inline]
pub(crate) fn pack_ref(off: u32, len: u32) -> Datum {
    // Full 8-byte Datum word (u64, not usize: a usize shift truncates on 32-bit wasm).
    Datum::from_u64(((off as u64) << 32) | len as u64)
}

pub(crate) fn stored_key_bytes<'a>(kind: CCFastKind, probe: &'a CatCKey<'_>) -> &'a [u8] {
    let b = probe.bytes();
    match kind {
        CCFastKind::Name => &b[..b.len().min(compute::NAMEDATALEN - 1)],
        _ => b,
    }
}

/// # Safety
/// `key` was written by `pack_ref` against this entry's live `payload`
/// allocation (insert-time invariant: `off + len <= payload_len`).
#[inline]
pub(crate) unsafe fn stored_bytes<'a>(payload: *const u8, key: Datum) -> &'a [u8] {
    let w = key.as_u64();
    let off = (w >> 32) as u32;
    let len = (w & 0xFFFF_FFFF) as u32;
    unsafe { core::slice::from_raw_parts(payload.add(off as usize), len as usize) }
}

#[inline(always)]
pub(crate) fn eq_stored(kind: CCFastKind, stored: Datum, payload: *const u8, probe: &CatCKey<'_>) -> bool {
    match kind {
        CCFastKind::Char | CCFastKind::Int2 | CCFastKind::Int4 => {
            stored.as_i32() == probe.word().as_i32()
        }
        _ => eq_stored_bytes_outlined(kind, stored, payload, probe),
    }
}

#[inline(never)]
fn eq_stored_bytes_outlined(
    kind: CCFastKind,
    stored: Datum,
    payload: *const u8,
    probe: &CatCKey<'_>,
) -> bool {
    match kind {
        // SAFETY: stored by-ref keys always pack a live in-payload slice.
        CCFastKind::Name => compute::name_eq(unsafe { stored_bytes(payload, stored) }, probe.bytes()),
        CCFastKind::Text | CCFastKind::OidVector => {
            // SAFETY: as above.
            let s = unsafe { stored_bytes(payload, stored) };
            s == probe.bytes()
        }
        _ => unreachable!("word kinds are handled inline"),
    }
}

#[allow(dead_code)]
#[inline]
pub(crate) fn compare_tuple(
    kinds: &[CCFastKind; 4],
    nkeys: i32,
    ct: &CatCTup,
    probes: &[CatCKey<'_>; 4],
) -> bool {
    for i in 0..nkeys as usize {
        if !eq_stored(kinds[i], ct.keys[i], ct.payload, &probes[i]) {
            return false;
        }
    }
    true
}

pub(crate) fn payload_alloc(mcx: Mcx<'_>, len: usize) -> types_error::PgResult<NonNull<u8>> {
    use mcx::Allocator;
    let layout = core::alloc::Layout::from_size_align(len.max(1), 8).unwrap();
    Ok(mcx.allocate(layout).map_err(|_| Box::new(mcx.oom(len)))?.cast())
}

pub(crate) fn payload_free(mcx: Mcx<'_>, ptr: *mut u8, len: u32) {
    use mcx::Allocator;
    if ptr.is_null() {
        return;
    }
    let layout = core::alloc::Layout::from_size_align((len as usize).max(1), 8).unwrap();
    // SAFETY: `ptr` came from `payload_alloc(mcx, len)` and is freed once
    // (CatCacheRemoveCTup/CList is the only caller and clears the slot).
    unsafe { mcx.deallocate(NonNull::new_unchecked(ptr), layout) };
}

pub struct CatCacheCensusRow {
    pub id: i32,
    pub relname: Option<String>,
    pub ntup: i32,
    pub nlist: i32,
    pub pinned: i32,
    pub shared: i32,
    pub payload_bytes: usize,
    pub arena_bytes: usize,
}

pub fn CatCacheCensus() -> (i32, Vec<CatCacheCensusRow>) {
    with_state(|st| {
        let mut rows = Vec::new();
        for cache in st.caches.iter().flatten() {
            let mut payload = 0usize;
            let mut pinned = 0i32;
            let mut shared = 0i32;
            for ct in cache.tuples.iter() {
                if !ct.payload.is_null() {
                    if ct.shared.is_some() {
                        shared += 1;
                    } else {
                        payload += ct.payload_len as usize;
                    }
                    if ct.refcount > 0 || ct.c_list != NONE {
                        pinned += 1;
                    }
                }
            }
            for cl in cache.lists.iter() {
                if !cl.payload.is_null() {
                    payload += cl.payload_len as usize;
                }
            }
            rows.push(CatCacheCensusRow {
                id: cache.id,
                relname: cache.cc_relname.as_ref().map(|s| s.as_str().to_string()),
                ntup: cache.cc_ntup,
                nlist: cache.cc_nlist,
                pinned,
                shared,
                payload_bytes: payload,
                arena_bytes: cache.tuples.capacity() * core::mem::size_of::<CatCTup>()
                    + cache.lists.capacity() * core::mem::size_of::<CatCList<'_>>()
                    + (cache.cc_bucket.capacity() + cache.cc_lbucket.capacity()) * 4,
            });
        }
        (st.ch_ntup, rows)
    })
}

pub fn init_seams() {
    catcache_seams::prepare_to_invalidate_cache_tuple::set(inval::PrepareToInvalidateCacheTuple);
    catcache_seams::catalog_cache_flush_catalog::set(graph::CatalogCacheFlushCatalog);
    catcache_seams::reset_catalog_caches_ext::set(graph::ResetCatalogCachesExt);
}
