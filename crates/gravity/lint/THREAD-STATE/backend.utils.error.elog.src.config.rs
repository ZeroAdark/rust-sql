backend/utils/error/elog/src/config.rs	LOG_MIN_MESSAGES	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	CLIENT_MIN_MESSAGES	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	CLIENT_AUTH_IN_PROGRESS	pgport	at:PerformAuthentication@backend/utils/init/postinit/src/lib.rs	thread	C's ClientAuthInProgress: true only while this backend thread authenticates its own client (postinit PerformAuthentication, backend_startup); a pool thread never authenticates, so a claim reads false, as its leader past authentication would
backend/utils/error/elog/src/config.rs	LOG_MIN_ERROR_STATEMENT	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	POSTMASTER_CONTEXT	pgport	scope:PostmasterContext	thread	set while a PostmasterContext guard lives on this thread (the postmaster's own reports); its Drop puts the flag and the output destination back
backend/utils/error/elog/src/config.rs	LOG_ERROR_VERBOSITY	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	LOG_LINE_PREFIX	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	LOG_DESTINATION	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	LOG_DESTINATION_STRING	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	SYSLOG_SEQUENCE_NUMBERS	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	SYSLOG_SPLIT_MESSAGES	pgport	claim	claim	gucs
backend/utils/error/elog/src/config.rs	PROC_EXIT_INPROGRESS	pgport	at:reset_exit_state_for_retained_park@backend/storage/ipc/ipc/src/lib.rs	thread	C's proc_exit_inprogress: set by this thread's own proc_exit, put back when a retained thread parks for its next task
backend/utils/error/elog/src/config.rs	AM_SYSLOGGER	pgport	exit	thread	MyBackendType == B_LOGGER: set once by the syslogger thread at its start (syslogger set_am_syslogger)
backend/utils/error/elog/src/config.rs	OUTPUT_FILE_NAME	pgport	exit	thread	C's OutputFileName (a standalone backend's -r switch), set at the thread's start; bounded: one path
backend/utils/error/elog/src/config.rs	BACKTRACE_FUNCTION_LIST	pgport	claim	claim	gucs
