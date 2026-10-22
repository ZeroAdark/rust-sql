backend/access/transam/transam_xlog/src/write.rs	LOGWRT_RESULT	pgport	exit	thread	C LogwrtResult: this thread's last-seen copy of the shared write / flush pointers, refreshed from XLogCtl (RefreshXLogWriteResult) before each decision; a stale copy is only behind
backend/access/transam/transam_xlog/src/write.rs	OPEN_LOG_FILE	pgport	at:XLogFileClose	thread	C openLogFile: the WAL segment this thread has open, closed by XLogFileClose before the next one; the VfsFd's Drop closes it at thread death
backend/access/transam/transam_xlog/src/write.rs	OPEN_LOG_SEG_NO	pgport	exit	thread	C openLogSegNo: the segment OPEN_LOG_FILE holds
backend/access/transam/transam_xlog/src/write.rs	OPEN_LOG_TLI	pgport	exit	thread	C openLogTLI: the timeline of the segment OPEN_LOG_FILE holds
backend/access/transam/transam_xlog/src/write.rs	LOCAL_MIN_RECOVERY_POINT	pgport	exit	thread	C LocalMinRecoveryPoint: this thread's copy of the control file's minRecoveryPoint, refreshed by UpdateMinRecoveryPoint
backend/access/transam/transam_xlog/src/write.rs	LOCAL_MIN_RECOVERY_POINT_TLI	pgport	exit	thread	C LocalMinRecoveryPointTLI: its timeline, refreshed with LOCAL_MIN_RECOVERY_POINT
backend/access/transam/transam_xlog/src/write.rs	UPDATE_MIN_RECOVERY_POINT	pgport	exit	thread	C updateMinRecoveryPoint: true until this thread sees recovery over (or a crash-recovery startup turns it off)
backend/access/transam/transam_xlog/src/write.rs	WAKE_WAL_SENDERS	pgport	at:WalSndWakeupProcessRequests	thread	C wake_wal_senders: set by this thread's XLogWrite (WalSndWakeupRequest), consumed and cleared by WalSndWakeupProcessRequests after its flush
backend/access/transam/transam_xlog/src/write.rs	REPORTED	pgport	test	test	write.rs's #[cfg(test)] mod file_error_tests: the PANIC diagnostic its emit-log hook captures
