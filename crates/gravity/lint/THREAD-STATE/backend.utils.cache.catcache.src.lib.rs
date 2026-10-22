backend/utils/cache/catcache/src/lib.rs	CAP_OVERRIDE	pgport	test	test	a test and tuning override: #[cfg(test)] only, set and cleared by a catcache unit test (census carry thread corrected to test)
backend/utils/cache/catcache/src/lib.rs	STATE	pgport	at:ResetCatalogCachesExt@backend/utils/cache/catcache/src/graph.rs	leader	session envelope; a pool thread registers no catalog caches; the catcache, reset by ResetCatalogCaches (invalidation), taken and freed by the session cleanup state_init registers
backend/utils/cache/catcache/src/lib.rs	BORROW_DEPTH	pgport	scope:BorrowGuard	leader	session envelope; debug re-entry check: with_state sets it, BorrowGuard's drop puts it back
backend/utils/cache/catcache/src/lib.rs	POOL_STRICT	claim	claim	claim	catalog
