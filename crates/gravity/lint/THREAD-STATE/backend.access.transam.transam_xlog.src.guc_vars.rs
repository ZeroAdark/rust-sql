backend/access/transam/transam_xlog/src/guc_vars.rs	MAX_WAL_SIZE_MB	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (max_wal_size, SIGHUP); its derived CheckPointSegments is a process-wide atomic
backend/access/transam/transam_xlog/src/guc_vars.rs	MIN_WAL_SIZE_MB	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (min_wal_size, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_KEEP_SIZE_MB	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_keep_size, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	XLOG_BUFFERS	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_buffers, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	XLOG_ARCHIVE_TIMEOUT	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (archive_timeout, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	XLOG_ARCHIVE_MODE	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (archive_mode, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	ENABLE_HOT_STANDBY	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (hot_standby, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	FULL_PAGE_WRITES	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (full_page_writes, SIGHUP); WAL inserts read the shared Insert.fullPageWrites, not this cell
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_LOG_HINTS	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_log_hints, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	LOG_CHECKPOINTS	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (log_checkpoints, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_SYNC_METHOD	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_LEVEL	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_level, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_RETRIEVE_RETRY_INTERVAL	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_retrieve_retry_interval, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	MAX_SLOT_WAL_KEEP_SIZE_MB	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (max_slot_wal_keep_size, SIGHUP)
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_DECODE_BUFFER_SIZE	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried; xlog_guc! storage per thread (wal_decode_buffer_size, POSTMASTER)
backend/access/transam/transam_xlog/src/guc_vars.rs	XLOG_ARCHIVE_COMMAND	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried (archive_command, SIGHUP): a leaked &'static str, replaced at each reload
backend/access/transam/transam_xlog/src/guc_vars.rs	CHECKPOINT_COMPLETION_TARGET	pgport	exit	thread	a POSTMASTER / SIGHUP setting, process-wide, never carried (checkpoint_completion_target, SIGHUP); its derived CheckPointSegments is a process-wide atomic
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_CONSISTENCY_CHECKING_STRING	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_COMPRESSION	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_INIT_ZERO	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	WAL_RECYCLE	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	COMMIT_DELAY	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	COMMIT_SIBLINGS	pgport	claim	claim	gucs
backend/access/transam/transam_xlog/src/guc_vars.rs	TRACK_WAL_IO_TIMING	pgport	claim	claim	gucs
