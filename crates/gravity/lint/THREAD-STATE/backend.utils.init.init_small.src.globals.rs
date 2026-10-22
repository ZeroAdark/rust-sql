backend/utils/init/init_small/src/globals.rs	PROC_SIGNAL_BARRIER_PENDING	pgport	at:CleanupProcSignalState@backend/storage/ipc/procsignal/src/lib.rs	thread	the thread's pending ProcSignalBarrier (procsignal.c's ProcSignalBarrierPending): HandleProcSignalBarrierInterrupt and ResetProcSignalBarrierBits set it, ProcessProcSignalBarrier clears it, and CleanupProcSignalState clears it with the slot (D4208), so a thread that lives on without a slot keeps no barrier to absorb
backend/utils/init/init_small/src/globals.rs	CTX	pgport	exit	thread	this thread's TopMemoryContext (mcx::session_root), made on first use; every pool thread has its own
backend/utils/init/init_small/src/globals.rs	INTERRUPT_PENDING	pgport	exit	thread	this thread's leaked InterruptPending flag; signal/timeout senders write through it; a claim's CFI reads its pool thread's own
backend/utils/init/init_small/src/globals.rs	ABORT_PENDING	pgport	exit	thread	this thread's kernel interrupt byte; a claim polls its LEADER's byte by the address its Binding carries, not this pool-thread cell
backend/utils/init/init_small/src/globals.rs	DUE_POLL	pgport	at:SetDuePoll	thread	wasm32 only (one thread, no pool): the armed wake's deadline check; SetDuePoll arms/disarms it
backend/utils/init/init_small/src/globals.rs	DUE_SKIP	pgport	at:SetDuePoll	thread	wasm32 only: InterruptPending's clock-read countdown; due_passed decrements, SetDuePoll zeroes
backend/utils/init/init_small/src/globals.rs	HOST_CANCEL	pgport	exit	thread	wasm32 only: embedder has a cancel channel; set once by SetHostCancel at backend start, never cleared
backend/utils/init/init_small/src/globals.rs	HOST_CANCEL_SEEN	pgport	at:TakeHostCancel	thread	wasm32 only: an embedder cancel PollHostCancel read, taken by ProcessInterrupts
backend/utils/init/init_small/src/globals.rs	INTERRUPTS_RAISED	pgport	exit	thread	monotonic count of errors ProcessInterrupts raised on this thread; read as marks by Gravity admission on the leader
backend/utils/init/init_small/src/globals.rs	LAST_INTERRUPT	pgport	at:ForgetInterruptRaised	thread	last error ProcessInterrupts raised here; forgotten at xact abort / statement start; a claim's CFI notes into its pool thread's own
backend/utils/init/init_small/src/globals.rs	REDELIVER	pgport	at:SetRedeliverInterrupts	thread	admission depth on the leader (admit set/restore is a bare pair); zeroed at xact abort / statement start; pool threads read 0
backend/utils/init/init_small/src/globals.rs	MY_CLIENT_SOCKET	pgport	exit	leader	the client socket, set at child launch, read once by backend startup; a pool thread has none
backend/utils/init/init_small/src/globals.rs	MY_PROC_PORT	pgport	at:SessionMemTeardownPort	thread	session Port; elog log_line_prefix reads it on any thread; a pool thread has none (as a PG parallel worker's MyProcPort is NULL)
backend/utils/init/init_small/src/globals.rs	PARSED_AUTH_LINES	pgport	exit	leader	hba/ident lines inherited at launch (Inherited copies them to every spawned thread); read only by authentication
backend/utils/init/init_small/src/globals.rs	PORT_LOG_VIEW	pgport	scope:WithMyProcPortLogged	thread	log_line_prefix view during ClientAuthentication, cleared by ClearView's drop; a pool thread always reads None
backend/utils/init/init_small/src/globals.rs	DATA_DIR	pgport	exit	thread	process-wide data directory (SetDataDir at postmaster start), copied to every spawned thread incl. rtpool (InheritDataDir); never carried
backend/utils/init/init_small/src/globals.rs	DATABASE_PATH	pgport	at:TakeDatabasePath	leader	the session database's path (InitPostgres); taken at a recovery invalidation (ClearDatabasePath seam); relmapper and the relcache init file read it on the backend only
backend/utils/init/init_small/src/globals.rs	FRONTEND_PROTOCOL	pgport	exit	leader	client protocol version (ProcessStartupPacket); read by startup and elog's frontend send; pool threads start at DestNone
backend/utils/init/init_small/src/globals.rs	QUERY_CANCEL_PENDING	pgport	at:ClearQueryCancelPending	thread	this thread's cancel flag; ProcessInterrupts on a pool thread reads its own; the leader's cancel reaches claims via the abort byte
backend/utils/init/init_small/src/globals.rs	PROC_DIE_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's SIGTERM flag (die), cleared by ProcessInterrupts / commit_to_exit
backend/utils/init/init_small/src/globals.rs	CHECK_CLIENT_CONNECTION_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	set by this thread's ClientCheckTimeoutHandler, cleared by ProcessInterrupts
backend/utils/init/init_small/src/globals.rs	CLIENT_CONNECTION_LOST	pgport	exit	thread	set when this thread's client socket fails; never cleared (the session FATALs)
backend/utils/init/init_small/src/globals.rs	IDLE_IN_TRANSACTION_SESSION_TIMEOUT_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's timeout flag; its handler sets, ProcessInterrupts clears
backend/utils/init/init_small/src/globals.rs	TRANSACTION_TIMEOUT_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's timeout flag; its handler sets, ProcessInterrupts clears
backend/utils/init/init_small/src/globals.rs	IDLE_SESSION_TIMEOUT_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's timeout flag; its handler sets, ProcessInterrupts clears
backend/utils/init/init_small/src/globals.rs	LOG_MEMORY_CONTEXT_PENDING	pgport	at:process_log_memory_context_interrupt@backend/utils/mmgr/mcxt_stats/src/lib.rs	thread	this thread's memory-context-dump request
backend/utils/init/init_small/src/globals.rs	PARALLEL_MESSAGE_PENDING	pgport	at:ProcessParallelMessages@backend/access/transam/parallel/src/lib.rs	thread	this thread's parallel-message flag (PG parallel leader)
backend/utils/init/init_small/src/globals.rs	SLOT_SYNC_SHUTDOWN_PENDING	pgport	at:ProcessSlotSyncMessage@backend/replication/slotsync/src/lib.rs	thread	slotsync worker's shutdown request, on its own thread
backend/utils/init/init_small/src/globals.rs	IDLE_STATS_UPDATE_TIMEOUT_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's timeout flag; its handler sets, ProcessInterrupts clears
backend/utils/init/init_small/src/globals.rs	IDLE_PASSIVATE_TIMEOUT_PENDING	pgport	at:process_interrupts@backend/tcop/postgres/src/lib.rs	thread	this thread's passivation timeout flag; handler sets, ProcessInterrupts / main loop clear
backend/utils/init/init_small/src/globals.rs	INTERRUPT_HOLDOFF_COUNT	pgport	at:error_recovery@backend/tcop/postgres/src/main_loop.rs	thread	this thread's HOLD_INTERRUPTS nesting; zeroed by error recovery (main_loop.rs error_recovery, commit_to_exit, parallel query_task_guard); the Gravity claim path zeroes none
backend/utils/init/init_small/src/globals.rs	QUERY_CANCEL_HOLDOFF_COUNT	pgport	at:error_recovery@backend/tcop/postgres/src/main_loop.rs	thread	this thread's HOLD_CANCEL_INTERRUPTS nesting; zeroed as above
backend/utils/init/init_small/src/globals.rs	CRIT_SECTION_COUNT	pgport	at:error_recovery@backend/tcop/postgres/src/main_loop.rs	thread	this thread's critical-section nesting; zeroed as above
backend/utils/init/init_small/src/globals.rs	MY_PROC_PID	pgport	exit	thread	this thread's synthetic pid (InitProcessGlobals via InitPostmasterChild; rtpool: pooldb_thread_main); log %p reads the thread's own
backend/utils/init/init_small/src/globals.rs	MY_LATCH	pgport	at:release_process_local_latch@backend/utils/init/miscinit/src/process.rs	thread	this thread's latch (local, then PGPROC's); a claim's WaitLatch waits on its pool thread's
backend/utils/init/init_small/src/globals.rs	MY_START_TIME	pgport	exit	thread	this thread's start (InitProcessGlobals); log %s/%c from a claim show the pool thread's, as a PG worker's own
backend/utils/init/init_small/src/globals.rs	MY_START_TIMESTAMP	pgport	exit	thread	this thread's start timestamp; backend_status / pgstat session time read the thread's own
backend/utils/init/init_small/src/globals.rs	MY_CANCEL_KEY	pgport	exit	leader	client cancel key (BackendKeyData), set by PostgresMain; pool threads have none
backend/utils/init/init_small/src/globals.rs	MY_CANCEL_KEY_LENGTH	pgport	exit	leader	length of the client cancel key, set with it
backend/utils/init/init_small/src/globals.rs	MY_PM_CHILD_SLOT	pgport	exit	thread	this child's postmaster slot (run_child_task); pmsignal reads the thread's own; rtpool threads have 0
backend/utils/init/init_small/src/globals.rs	OUTPUT_FILE_NAME	pgport	exit	thread	-o switch path, process-wide, copied at spawn (Inherited); no other reader
backend/utils/init/init_small/src/globals.rs	MY_EXEC_PATH	pgport	exit	thread	process-wide exec path (getInstallationPaths), copied to every spawned thread; pgtz/tzparser read it anywhere
backend/utils/init/init_small/src/globals.rs	PKGLIB_PATH	pgport	exit	thread	process-wide $libdir, copied to every spawned thread; dfmgr reads it anywhere
backend/utils/init/init_small/src/globals.rs	MY_PROC_NUMBER	pgport	at:ProcKill@backend/storage/lmgr/lmgr_proc/src/lib.rs	thread	this thread's PGPROC (InitProcess; rtpool at the serve gate); claim buffer/AIO waits use the pool thread's own
backend/utils/init/init_small/src/globals.rs	PARALLEL_LEADER_PROC_NUMBER	pgport	at:reset_worker_identity@backend/access/transam/parallel/src/lib.rs	thread	set by ParallelWorkerMain / standing serve_ticket; Gravity claims leave it INVALID; temp-rel readers never run in a claim
backend/utils/init/init_small/src/globals.rs	MY_DATABASE_ID	pgport	claim	claim	database
backend/utils/init/init_small/src/globals.rs	PGSTAT_REPORT_FIXED	pgport	at:report_stat_slow@backend/utils/activity/pgstat/src/pending.rs	thread	this thread's has-fixed-stats flag (WAL insert, report_fixed_set); claim IO is folded on the leader
backend/utils/init/init_small/src/globals.rs	MY_DATABASE_TABLE_SPACE	pgport	claim	claim	database
backend/utils/init/init_small/src/globals.rs	MY_DATABASE_HAS_LOGIN_EVENT_TRIGGERS	pgport	exit	leader	session DB flag (InitPostgres); read at login only
backend/utils/init/init_small/src/globals.rs	POSTMASTER_PID	pgport	exit	thread	process-wide, set at postmaster start, copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	IS_POSTMASTER_ENVIRONMENT	pgport	exit	thread	process-wide, copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	IS_UNDER_POSTMASTER	pgport	exit	thread	set per child by InitPostmasterChild (rtpool: pooldb prelude, default on); not in Inherited, so false on pool threads under POOLDB=0
backend/utils/init/init_small/src/globals.rs	IS_BINARY_UPGRADE	pgport	exit	thread	process-wide -b flag, copied at spawn (Inherited); readers are DDL/catalog on the leader
backend/utils/init/init_small/src/globals.rs	DATE_STYLE	pgport	exit	thread	a dead mirror: the DateStyle backing is adt_datetime settings.rs DATE_STYLE (carried by gucs); only Inherited and sim_net copy this cell
backend/utils/init/init_small/src/globals.rs	DATE_ORDER	pgport	exit	thread	a dead mirror of adt_datetime settings.rs DATE_ORDER; only Inherited and sim_net copy this cell
backend/utils/init/init_small/src/globals.rs	INTERVAL_STYLE	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	ENABLE_FSYNC	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	ALLOW_SYSTEM_TABLE_MODS	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	WORK_MEM	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	HASH_MEM_MULTIPLIER	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	MAINTENANCE_WORK_MEM	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	MAX_PARALLEL_MAINTENANCE_WORKERS	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	N_BUFFERS	pgport	exit	thread	shared_buffers (POSTMASTER), process-wide, never carried; copied to every spawned thread (Inherited)
backend/utils/init/init_small/src/globals.rs	MAX_CONNECTIONS	pgport	exit	thread	POSTMASTER, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	MAX_WORKER_PROCESSES	pgport	exit	thread	POSTMASTER, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	MAX_PARALLEL_WORKERS	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	MAX_BACKENDS	pgport	exit	thread	computed once by InitializeMaxBackends, copied to every spawned thread (Inherited); multixact/procarray sizing reads it anywhere
backend/utils/init/init_small/src/globals.rs	RUNTIME_GANG_PROCS	pgport	exit	thread	set once by the postmaster before InitializeMaxBackends, copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	VACUUM_BUFFER_USAGE_LIMIT	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_PAGE_HIT	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_PAGE_MISS	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_PAGE_DIRTY	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_LIMIT	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_DELAY	pgport	claim	claim	gucs
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_BALANCE	pgport	at:reset_vacuum_cost_balances@backend/commands/vacuum/src/lib.rs	thread	this thread's vacuum I/O cost accrual (bufmgr, while VacuumCostActive); a Gravity claim accrues none
backend/utils/init/init_small/src/globals.rs	VACUUM_COST_ACTIVE	pgport	at:VacuumFinally@backend/commands/vacuum/src/lib.rs	thread	this thread's cost-delay switch (VacuumUpdateCosts); the VACUUM / ANALYZE window's PG_FINALLY guard clears it on every exit; false on Gravity pool claims
backend/utils/init/init_small/src/globals.rs	AUTOVACUUM_FREEZE_MAX_AGE	pgport	exit	thread	POSTMASTER, never carried; NOT in Inherited: a thread gets it from its GUC prelude only
backend/utils/init/init_small/src/globals.rs	COMMIT_TIMESTAMP_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	MULTIXACT_MEMBER_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	MULTIXACT_OFFSET_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	NOTIFY_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	SERIALIZABLE_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	SUBTRANSACTION_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
backend/utils/init/init_small/src/globals.rs	TRANSACTION_BUFFERS	pgport	exit	thread	POSTMASTER SLRU sizing, process-wide, never carried; copied at spawn (Inherited)
