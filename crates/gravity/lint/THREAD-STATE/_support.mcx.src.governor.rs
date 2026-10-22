_support/mcx/src/governor.rs	ARMED	memory	claim	claim	statement_memory
_support/mcx/src/governor.rs	SLOT	memory	exit	thread	memory crate, census-only: this thread's governor slot index, bound by register to its backend's ProcNumber
_support/mcx/src/governor.rs	LEDGER	memory	claim	claim	statement_memory
_support/mcx/src/governor.rs	OWN	memory	statement:statement_start	leader	memory crate, census-only: the backend's own statement ledger, zeroed and re-armed by statement_start, off at statement_end
_support/mcx/src/governor.rs	PENDING	memory	at:flush_pending	thread	memory crate, census-only: this thread's pending charges, taken by flush_pending (statement start / end, enter / exit)
