backend/commands/event_trigger/src/lib.rs	CURRENT_STATE	pgport	scope:EventTriggerCleanup@backend/tcop/utility/src/dispatch.rs	leader	C's currentEventTriggerState stack: pushed by EventTriggerBeginCompleteQuery (its one caller, ProcessUtilitySlow's dispatch) and popped by the EventTriggerCleanup guard's EventTriggerEndCompleteQuery (C's PG_FINALLY); read by DDL collection and the event-trigger SRFs (proparallel r)
backend/commands/event_trigger/src/lib.rs	EVENT_TRIGGERS_GUC	pgport	claim	claim	gucs
backend/commands/event_trigger/src/lib.rs	SESSION_REPLICATION_ROLE	pgport	claim	claim	gucs
