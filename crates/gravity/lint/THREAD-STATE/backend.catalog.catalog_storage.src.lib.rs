backend/catalog/catalog_storage/src/lib.rs	PENDING	pgport	xact:smgrDoPendingDeletes	leader	C's pendingDeletes: the relation files to drop or keep at this transaction's (or subtransaction's) end
backend/catalog/catalog_storage/src/lib.rs	PENDING_SYNCS	pgport	xact:smgrDoPendingSyncs	leader	C's pendingSyncHash: the relations made in this transaction with WAL skipped, synced at its commit
backend/catalog/catalog_storage/src/lib.rs	WAL_SKIP_THRESHOLD	pgport	at:init_seams	leader	the wal_skip_threshold GUC's backing (PGC_USERSET), read only by smgrDoPendingSyncs at the leader's commit
