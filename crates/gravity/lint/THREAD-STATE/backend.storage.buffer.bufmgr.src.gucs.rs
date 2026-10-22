backend/storage/buffer/bufmgr/src/gucs.rs	CHECKPOINT_FLUSH_AFTER	pgport	at:install_guc_backing	thread	checkpoint_flush_after (SIGHUP): process-wide, never carried; GUC storage per thread, written by the set accessor install_guc_backing installs (config reload), read by the checkpointer's own writeback context
backend/storage/buffer/bufmgr/src/gucs.rs	BGWRITER_FLUSH_AFTER	pgport	at:install_guc_backing	thread	bgwriter_flush_after (SIGHUP): process-wide, never carried; GUC storage per thread, written by the set accessor install_guc_backing installs (config reload), read by the bgwriter's own writeback context
backend/storage/buffer/bufmgr/src/gucs.rs	BGWRITER_LRU_MAXPAGES	pgport	at:install_guc_backing	thread	bgwriter_lru_maxpages (SIGHUP): process-wide, never carried; GUC storage per thread, written by the set accessor install_guc_backing installs (config reload), read by BgBufferSync on the bgwriter
backend/storage/buffer/bufmgr/src/gucs.rs	BGWRITER_LRU_MULTIPLIER	pgport	at:install_guc_backing	thread	bgwriter_lru_multiplier (SIGHUP): process-wide, never carried; GUC storage per thread, written by the set accessor install_guc_backing installs (config reload), read by BgBufferSync on the bgwriter
backend/storage/buffer/bufmgr/src/gucs.rs	EFFECTIVE_IO_CONCURRENCY	pgport	claim	claim	gucs
backend/storage/buffer/bufmgr/src/gucs.rs	IO_COMBINE_LIMIT_GUC	pgport	claim	claim	gucs
backend/storage/buffer/bufmgr/src/gucs.rs	BACKEND_FLUSH_AFTER	pgport	claim	claim	gucs
backend/storage/buffer/bufmgr/src/gucs.rs	ZERO_DAMAGED_PAGES	pgport	claim	claim	gucs
backend/storage/buffer/bufmgr/src/gucs.rs	IGNORE_CHECKSUM_FAILURE	pgport	claim	claim	gucs
backend/storage/buffer/bufmgr/src/gucs.rs	TRACK_IO_TIMING	pgport	claim	claim	gucs
