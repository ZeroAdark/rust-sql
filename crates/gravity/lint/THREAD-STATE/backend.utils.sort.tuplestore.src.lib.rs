backend/utils/sort/tuplestore/src/lib.rs	SLOT	pgport	at:take	thread	one parked tuplestore shell of this thread, taken by its next begin; dropped by the session cleanup park registers
backend/utils/sort/tuplestore/src/lib.rs	CLEANUP_ARMED	pgport	at:park	thread	whether this thread's session cleanup for the parked shell is registered (park sets it, the cleanup clears it)
