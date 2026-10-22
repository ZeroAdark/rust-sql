backend/executor/runtime/src/sched.rs	LEDGER_GRANT	pgport	scope:GrantCtx	thread	the (scheduler, slot) of the ledger-joined task this thread runs; GrantCtx's Drop clears it, an unwind too
backend/executor/runtime/src/sched.rs	CALLER_DUTY	pgport	scope:CallerDutyCtx	thread	the claim-boundary duty hook of the session thread driving its own task set; CallerDutyCtx's Drop clears it, an unwind too
