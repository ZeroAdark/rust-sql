backend/utils/misc/timeout/src/lib.rs	DATA	pgport	at:InitializeTimeouts	thread	C's all_timeouts / active_timeouts: this thread's own registered and active timeouts, armed and fired on this thread (a claim's own lock waits arm them on its pool thread)
backend/utils/misc/timeout/src/lib.rs	ALL_TIMEOUTS_INITIALIZED	pgport	at:InitializeTimeouts	thread	C's all_timeouts_initialized, set by this thread's InitializeTimeouts
backend/utils/misc/timeout/src/lib.rs	ALARM	pgport	at:InitializeTimeouts	thread	this thread's alarm slot in the process timer (claim_alarm), taken once
backend/utils/misc/timeout/src/lib.rs	SIGNAL_DUE_AT	pgport	at:InitializeTimeouts	thread	C's signal_due_at for this thread's alarm
backend/utils/misc/timeout/src/lib.rs	POSTED	pgport	at:InitializeTimeouts	thread	this thread's slot flag, shared with the timer thread
backend/utils/misc/timeout/src/lib.rs	DUE_NS	pgport	at:InitializeTimeouts	thread	this thread's own copy of its abort-class wake deadline, fired by fire_due at the end of a run
backend/utils/misc/timeout/src/lib.rs	ARMED_DEADLINE	pgport	at:InitializeTimeouts	thread	the deadline of this thread's armed wake, until it is delivered
