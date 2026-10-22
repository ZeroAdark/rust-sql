backend/executor/runtime/src/io.rs	WORKER_RT	pgport	at:worker_exit	thread	the runtime this pool worker thread is bound to (worker_enter), cleared at worker_exit
backend/executor/runtime/src/io.rs	PERMIT_HELD	pgport	at:worker_exit	thread	whether this pool worker thread holds its execution permit now (the loop's bookkeeping), cleared at worker_exit
backend/executor/runtime/src/io.rs	IN_IO_SECTION	pgport	at:worker_exit	thread	whether this pool worker thread is inside a declared blocking section (permit donated), cleared at worker_exit
